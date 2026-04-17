# Changelog

All notable changes to Batty are documented here.

## 0.11.49 — 2026-04-17

Field-report fix: in batty-marketing at 12:42:49 UTC, a planning
cycle fired only 572s after the previous empty response (12:33:17)
despite the v0.11.25 (#687) backoff logic calling for a 900s
cooldown at `consecutive_empty=2` (3× base). Root cause: the
daemon was restarted at 12:34:03 by a forced kill (`batty stop`
timeout — "daemon did not stop gracefully; forcing shutdown"), so
the clean-shutdown `persist_runtime_state(true)` path never ran;
the last persisted snapshot was a 5-min heartbeat from before the
12:33:17 empty-response in-memory update. On resume, the restored
`consecutive_empty` was stale (1 instead of 2), shrinking the
effective cooldown from 900s → 600s and letting a fresh planning
cycle fire inside the real backoff window.

### Fixes

- **Empty planning response now persists immediately** (#710) —
  `handle_planning_response()` in `src/team/daemon/automation.rs`
  calls `persist_runtime_state(false)` right after it bumps
  `planning_cycle_consecutive_empty` / resets it to zero and
  re-anchors `planning_cycle_last_fired`. Mirrors the existing
  persist-after-fire call at the trigger site (#687 followup), so
  both sides of the planning-cycle lifecycle survive SIGKILL,
  panic, OOM, or an abrupt `batty stop` timeout. Regression test:
  `empty_planning_response_persists_consecutive_empty_increment`
  verifies the persisted state file carries the updated
  `consecutive_empty` and `last_fired_elapsed_secs` after an empty
  response.

## 0.11.48 — 2026-04-17

Field-report fix: in batty-marketing, jordan-pm's own note on task
#573 body captured the symptom: "this is the third planning-cycle
trigger in ~90min on stale 'idle engineers' diagnosis. ...
Dispatcher mis-reads policy-parked tasks as dispatch-blocked."
Each spurious planning cycle burns one Maya turn (~5k+ tokens), so
three false triggers in ~90min = ~15k tokens of waste. Root cause:
`dispatchable_task_count()` (called by tact planner + idle-burst
and utilization interventions) delegates to
`resolver::dispatchable_tasks()` which only filters on
runnability, not on who owns the task. Since #703 introduced
body-owner parsing that routes Maya-owned tasks away from
engineers, engineers-dispatchable and dispatchable diverged: a
Maya-owned runnable task still counted toward "engineer-
dispatchable" for planning-cycle trigger math
(`idle_engineers > dispatchable_tasks`), so idle engineers
appeared "starved" even when Maya had plenty of her own work
queued.

### Fixes

- **Planner and interventions now count only engineer-dispatchable
  tasks** (#709) — new `resolver::engineer_dispatchable_tasks()`
  helper layers #703's body-owner + assignee filter on top of
  `dispatchable_tasks()`, excluding tasks whose body-parsed
  `Owner:` line names a non-engineer member (architect, manager,
  reviewer) or whose frontmatter `assignee` is a non-engineer.
  Callers updated: `tact::dispatchable_task_count()`,
  `interventions::dispatch::IdleBurstCheck`, and
  `interventions::utilization::UtilizationCheck`. Exposed
  `parse_body_owner_role` as `pub(crate)` and raised `dispatch`
  and `dispatch::queue` module visibility to `pub(crate)` so the
  resolver can share the body-owner parser. Regression tests:
  `engineer_dispatchable_filters_out_maya_owned_body_tasks` (body
  `Owner:` line naming maya-planner excluded) and
  `engineer_dispatchable_filters_explicit_non_engineer_assignee`
  (frontmatter `assignee: maya-planner` excluded). Prevents
  repeated spurious planning cycles when engineers are idle but
  only planner-owned work remains.

## 0.11.47 — 2026-04-17

Field-report fix: in batty-marketing at 12:27:30 UTC, task #572
("Card-1 peak-day hero card", tags `[pillar-a, design, thread-a,
hero, card-1]`) was dispatched to **alex-dev-1-1** (engineer role,
not designer) instead of **sam-designer-1-1**. alex released it
within 38 s. #691's role-name seeding works for tasks tagged with
the literal role_name (`sam-designer`) but natural-language tags
like `design` still scored zero tag-overlap and fell through to
alphabetical tiebreaker.

### Fixes

- **Natural-language role tags (`design`, `writing`, `designer`)
  now route to the role-aligned engineer** (#708) — new
  `role_name_seed_tags()` helper in `src/team/dispatch/queue.rs`
  expands each engineer's role_name into seed tokens: the full
  role_name (`sam-designer`), its hyphen-suffix token
  (`designer`), and `-er` noun-agent stem/gerund variants
  (`design`, `designing`). Short stems (<3 chars) skipped to
  avoid noise. Example expansions: `sam-designer` →
  `[sam-designer, designer, design, designing]`; `priya-writer`
  → `[priya-writer, writer, writ, writing]`; `alex-dev` →
  `[alex-dev, dev]`. Regression tests:
  `dispatch_queue_seeds_role_name_word_family_variants` (full
  dispatch path for task #572's tag set) +
  `role_name_seed_tags_covers_hyphen_suffix_and_er_variants`
  (helper unit test).

## 0.11.46 — 2026-04-17

Field-report fix: in batty-marketing after the v0.11.45 daemon
restart at 12:06 UTC, engineers alex-dev-1-1 (Task #570) and
sam-designer-1-1 (Task #572) — both holding `active_tasks` claims
from before the restart — sat idle for 12+ minutes with
`output_bytes=0 uptime_secs=760` and no inbox messages delivered
since pre-restart. The intervention system eventually rescued them
via owned-task prodding at 12:18 UTC, but that delay is pure
token-waste the launcher should have prevented.

Root cause: `spawn_all_agents(resume)` runs in `run()` BEFORE
`restore_runtime_state()`, so `self.active_tasks` is empty during
`prepare_member_launch` — the existing handoff-based prompt
injection branch never fires for engineers whose claims survived
the restart. SDK mode compounds the gap: the role prompt is wired
into `--append-system-prompt` (a system prompt, not a user message),
so a fresh shim has no stimulus to act on.

### Fixes

- **Engineers with pre-restart active_tasks now receive an explicit
  resume prompt at daemon startup** (#707) — new
  `enqueue_restart_resume_prompts()` pass in `poll.rs::run()` iterates
  `self.active_tasks` after the orphan-reclaim block, loads each
  engineer's claimed board task, and queues
  `restart_assignment_message(task)` via `queue_message("daemon",
  engineer, …)`. Gated on `resume && !is_hot_reload` so clean
  `stop/start` cycles recover task context without waiting for
  intervention backoff. Regression tests seed an owned task and
  verify the inbox receives one "Continuing Task #N: …" message
  per pre-restart claim.

## 0.11.45 — 2026-04-17

Field-report fix: in batty-marketing jordan-pm reached `usage_pct=211`
(2,114,196 used / 1,000,000 bumped) at 11:49 UTC and kept running.
A stall-mid-turn retry then fired at 11:52:06 UTC with `attempt=3`
and the restart path silently no-op'd: `warn: "context pressure
restart requested but no active task is recorded member=jordan-pm
reason=stalled mid-turn"`. The oversized shim never recycled, and
`expiring stale pending messages to inbox fallback` fired every ~60s
because the shim was too slow draining its delivery queue at 2M-token
turns.

### Fixes

- **Managers/architects with no active task now cold-respawn on
  stall-retry instead of silently no-op'ing** (#706) —
  `restart_member_with_task_context` in
  `src/team/daemon/health/context_exhaustion.rs` falls back to
  task-less `restart_member` (pane respawn + fresh launch identity)
  when `active_task(member_name)` returns None. Managers and
  architects never claim board tasks, so the task-context path's
  checkpoint-handoff is meaningless for them — the correct recovery
  is plain pane respawn. Before this fix the stall-retry path
  (`handle_stalled_mid_turn_completion` attempt ≥ 3) would request a
  restart, hit the no-active-task guard, warn, and return Ok(()),
  leaving the manager stuck in a stall cascade with unbounded
  context growth. Regression test:
  `restart_member_with_task_context_falls_back_to_pane_respawn_for_manager_without_task`.

## 0.11.44 — 2026-04-17

Field-report fix: in batty-marketing the dispatcher cascade-bounced
task #549 ("Pillar B: Draft This Week in Rust submission") across
three engineers on three ticks because the task body's explicit
routing cue (`- Route: dispatch to priya-writer.`) was used only as
a scoring boost, not a hard eligibility gate. First dispatch at
09:13:35 UTC went to kai-devrel-1-1 (refused); 10:18:42 UTC went to
alex-dev-1-1 (refused); 11:04:36 UTC looped back to kai-devrel-1-1
(refused again) because priya-writer-1-1 was `working` at each tick.
Only jordan-pm's manual inbox-reroute at 11:04:49 UTC broke the
cycle. Three dispatch turns were burned on work that was always
priya's.

### Fixes

- **Body-owner routing is now a hard eligibility gate, not just a
  scoring boost** (#705) — `rank_dispatch_engineers` in
  `src/team/dispatch/queue.rs` now restricts `eligible` to engineers
  carrying the body-owner role_name when `parse_body_owner_role`
  identifies one, mirroring #682's `assignee:` frontmatter behavior.
  If no engineer with that role is idle, the task stays undispatched
  until one becomes available — no more cascade-dispatching to a
  peer who will immediately refuse. The gate is gated by role
  existence: bodies naming a non-engineer role still fall through
  to #703's filter in `available_dispatch_tasks`, and bodies naming
  an unknown/unconfigured role fall through to scoring as before.
  Regression tests:
  `enqueue_dispatch_candidates_waits_when_body_owner_engineer_busy`
  covers the #549 shape (named engineer busy → no dispatch);
  `enqueue_dispatch_candidates_dispatches_to_body_owner_when_idle_even_with_other_idle_peers`
  proves the gate also forces the named engineer to win when they
  are idle alongside other idle peers.

## 0.11.43 — 2026-04-17

Field-report fix: in batty-marketing, kai-devrel-1-1 drafted #528
(PILLAR A T+24h check-in template) and moved it to review at
~10:33 UTC for jordan-pm's audit. The very next reconciliation pass
at 10:35:16 UTC saw `status=review`, `review_owner=None`,
`actively_tracked` no longer contained #528 (the same pass had just
cleared kai's `active_tasks` entry with reason "task entered
review"), and bounced #528 back to todo. Jordan never got to audit —
by the time the pass fired, the review was already dismantled. This
was the second time in 12 minutes for #528 alone (also at 10:23:48),
and the same pattern has recurred 28 times across the current
daemon log, burning engineer redraft cycles on #498, #499, #504,
#506, #513, #521, #523, #525, #526, #528, #529, #537, #553, etc.

### Fixes

- **Orphan-review rescue now respects a 10-minute grace window
  before bouncing a fresh review** (#704) — `reconcile_active_tasks`
  in `src/team/daemon/automation.rs` now skips review-state rescue
  when the task file's mtime is within `REVIEW_RESCUE_GRACE_SECS`
  (600 s). Kanban-md edits the task file on every transition, so
  mtime is a reliable proxy for "last state change" — a task that
  just moved into review has mtime within seconds and should not be
  bounced until the manager/architect has had time to assign
  themselves as `review_owner`. Stale reviews (file untouched for
  longer than the grace window) continue to rescue exactly as
  before. Regression tests:
  `orphan_review_rescue_skips_freshly_transitioned_reviews` covers
  the #528 shape, and `orphan_review_rescue_fires_on_stale_reviews`
  back-dates mtime by 1 hour to prove the rescue still fires for
  genuinely stale reviews.

## 0.11.42 — 2026-04-17

Field-report fix: in the batty-marketing observation window a task
whose body named the architect as the owner was dispatched to an
engineer and immediately auto-refused. At 10:18:42 UTC the dispatch
loop assigned #542 ("STRATEGY — Star-velocity Tue 04-21 mid-window
gate decision", body line 1 `**Owner:** maya-lead (this task)`) to
`sam-designer-1-1` — a designer with no context for a strategy
decision. `assignee:` frontmatter was unset, so the #682 filter
passed the task through; `rank_dispatch_engineers` then spliced the
`maya-lead` role into task tags for scoring, but no engineer carries
that role, so tag_overlap scored 0 for every candidate and the task
landed on whichever engineer won tiebreakers. Sam burned a claim +
refuse turn on work that shouldn't have entered the dispatch pool
at all.

### Fixes

- **Body-owner declarations naming non-engineers are now excluded
  from dispatch** (#703) — `available_dispatch_tasks` in
  `src/team/dispatch/queue.rs` gained a filter that runs
  `parse_body_owner_role` against the task description and drops
  the task when the parsed role matches a configured non-engineer
  member (architect, manager, human). The filter only applies when
  `assignee:` is unset, so an explicit engineer assignee still
  wins. Complements #682 (frontmatter `assignee:` non-engineer
  filter) — same wrong-role-dispatch family, different signal
  source. Regression tests:
  `enqueue_dispatch_candidates_skips_tasks_whose_body_owner_names_non_engineer`
  covers the #542 shape; the companion
  `enqueue_dispatch_candidates_allows_body_owner_when_it_names_an_engineer`
  proves the filter is surgical (engineer-owner bodies still
  dispatch).

## 0.11.41 — 2026-04-17

Field-report fix: in the batty-marketing observation window the
architect utilization intervention fired in the same tick as the
owned-task intervention for the same engineer, double-nudging the
architect about a condition the engineer was already being asked
to address directly. At 09:52:10 UTC `maybe_intervene_owned_tasks`
queued a nudge to `alex-dev-1-1` for his in-progress #518, and
105 ms later `maybe_intervene_architect_utilization` fired for
`maya-lead` with `idle_active_engineers=1` — listing the same
alex/#518 pair. 90 seconds after maya responded, `tact_check`
fired a planning cycle with `dispatchable_tasks=0`, stacking a
third overlapping nudge on the architect inbox. The architect
burned supervisory turns on information the engineer was already
acting on, and the owned-task state machine already has an
escalation path (`escalation_sent`) for when the direct nudge
fails.

### Fixes

- **Suppress architect utilization during active owned-task
  nudge** (#702) — `maybe_intervene_architect_utilization` in
  `src/team/daemon/interventions/utilization.rs` now filters
  `idle_active_engineers` to exclude engineers whose
  `owned_task_interventions` entry has `escalation_sent=false`.
  The architect signal re-engages automatically once owned-task
  escalates to the manager (i.e. the engineer ignored the direct
  nudge and the supervisory chain is the right escalation
  target). Regression test:
  `architect_utilization_suppresses_idle_active_until_owned_task_escalates`
  covers both the suppressed-first-fire and post-escalation
  re-fire paths.

## 0.11.40 — 2026-04-17

Field-report fix: the zero-diff completion tracker was cold-
respawning engineers every two turns in non-git projects.
`maybe_track_zero_diff_completion` runs `git show-ref` /
`git rev-list` against `engineer-base..engineer-branch`; in a
non-git project both calls fail, `branch_exists` stays false,
`commits_ahead` stays 0, and the `branch_exists && commits_ahead
> 0` short-circuit never fires. The counter increments on every
engineer completion, crosses the threshold (2) in two events,
and the engineer gets cold-respawned losing ~200K tokens of
context. Observed in batty-marketing (a content-pipeline
project with no git repo): alex-dev-1-1 cold-respawned at
09:22:05 and 09:25:12 — every 3 minutes — on successful
completions with `response_len=936` / `response_len=161`. The
work-preservation path also errored with `permanent git error:
fatal: not a git repository` on each respawn.

### Fixes

- **Zero-diff completion tracker skips non-git and multi-repo
  projects** (#701) — `maybe_track_zero_diff_completion` in
  `src/team/daemon/health/poll_shim.rs` now returns `None`
  immediately when `!self.is_git_repo || self.is_multi_repo`,
  matching the same gate `main_smoke_check` uses. The mechanism
  relies on commit attribution against a single
  `<base>..<engineer-branch>` range — without that range it
  cannot tell success from failure and its default answer
  (failure) causes catastrophic context loss. Regression tests:
  `zero_diff_completion_tracking_skips_non_git_projects` and
  `zero_diff_completion_tracking_skips_multi_repo_projects`.

## 0.11.39 — 2026-04-17

Field-report fix: a second wrong-role dispatch incident in
batty-marketing at 09:13:35 UTC showed that the #699 fix only
covered the literal `Owner: <role>` shape. jordan-pm's tasks
author routing hints with richer syntax — `**Owner routing**:
... route to priya-writer ...`, `Primary: priya-writer`,
`- Route: dispatch to priya-writer` — and none of these
surface `Owner:` as a substring with a role right after. The
three-task wave misdispatched: #547 ("route to priya-writer")
to alex-dev, #548 ("Primary: priya-writer ... NOT Sam") to
sam-designer (the explicitly-excluded engineer), #549
("dispatch to priya-writer") to kai-devrel. jordan-pm then
spent a full turn reassigning each via inbox — workaround,
not fix. The #697 release-exclusion worked as intended
(refusing engineers were not re-picked for their own task),
but the underlying routing still landed on the wrong role.

### Fixes

- **Owner-role parser recognises `Route:`, `Primary:`, `Owner
  routing`, and inline prose cues** (#700) — expanded
  `ROUTING_CUES` in `parse_body_owner_role` to eight phrases:
  `Owner:`, `OWNER:`, `Route:`, `Primary:`, `Owner routing`,
  `route to`, `dispatch to`, `assign to`. Introduced
  `first_role_token_after` helper that scans for the first
  hyphenated lowercase token after each cue, with a
  `NON_ROLE_HYPHEN_TOKENS` blacklist so descriptive compounds
  (`role-flexible`, `strategic-analysis`, `narrative-audit`,
  `north-star`, `any-engineer`) can't be mistaken for role
  names. Token collection also trims trailing hyphens so
  instance-id suffixes like `alex-dev-1-1` normalise to the
  role `alex-dev` instead of being rejected for ending in `-`.
  Regression test
  `parse_body_owner_role_finds_routing_cues_from_jordan_style_bodies`
  covers #547/#548/#549 body shapes verbatim plus the
  manual-inbox `OWNER: alex-dev-1-1` workaround format.

## 0.11.38 — 2026-04-17

Field-report fix: Maya-style round headers were defeating the
body-owner-role parser. Tasks whose bodies begin
`**Round-8 task from maya-lead. Owner: alex-dev. …**` still got
dispatched to the wrong role — the owner hint was embedded
mid-line inside a prose preamble, and the parser only accepted
`Owner:` at the start of a line (after trimming leading `-` /
`*`). `strip_prefix("Owner:")` failed against "Round-8 task
from maya-lead.", the hint was ignored, and the four-way batch
dispatch at 08:45:23 UTC round-robined #518 ("Owner: alex-dev")
to sam-designer-1-1, #524 ("Owner: alex-dev") to kai-devrel-1-1,
and #517 ("Owner: kai-devrel") to priya-writer-1-1 in
batty-marketing. Each wrong-role engineer then had to burn a
turn refusing and blocking as a dispatcher-false-positive,
wasting four engineer contexts per wave replenishment.

### Fixes

- **Owner-role parser finds `Owner:` inside prose preambles** (#699)
  — `parse_body_owner_role` in `src/team/dispatch/queue.rs`
  now scans each line for `Owner:` anywhere, with a word-boundary
  guard so unrelated compounds like `CoOwner:` or `DataOwner:`
  can't masquerade as the hint. The existing line-start cases
  (`Owner: priya-writer …`, `- Owner: **kai-devrel** …`) still
  resolve via the same path; the new path catches Maya-style
  round headers that wrap the declaration inside prose
  (`**Round-8 task from maya-lead. Owner: alex-dev. …**`).
  Regression test
  `parse_body_owner_role_finds_owner_inside_prose_preamble`
  covers #518/#524 body shapes plus the CoOwner bypass guard.

## 0.11.37 — 2026-04-17

Field-report fix: the orphan rescue was quietly destroying
engineer-set block metadata. When a task carried
`blocked: true` + `block_reason: "..."` in frontmatter and the
engineer released their claim, the rescue's
`transition_task(..., "todo")` path called `clear_blocked`
(correct for manual `batty move` — "engineer is unblocking the
task" — but wrong for rescue — "daemon is cleaning up a stuck
claim, not revoking the block"). The block vanished, the task
became dispatchable again, and the next ranked engineer picked
it up. Observed in batty-marketing task #553: priya-writer
parked the task with an explicit `PARKED pending Maya
sequencing ruling` block, released the claim; within one tick
the rescue wiped the block; sam-designer-1-1 got re-dispatched
and auto-refused for the third time in ~40 minutes.

### Fixes

- **Orphan rescue preserves engineer-set block metadata** (#698)
  — new `transition_task_preserving_block` helper in
  `src/team/task_cmd.rs` that writes the status field but skips
  `clear_blocked`. `reconcile_active_tasks` in automation.rs
  now checks `task.blocked.is_some() || task.blocked_on.is_some()`
  before each rescue transition. When the task was blocked, the
  rescue uses the preserving variant; otherwise it falls through
  to the legacy `transition_task` so manual unblock semantics on
  non-blocked rescues are unchanged. Applies to both the
  "orphaned review" and "orphaned in-progress" branches.
  Combined with the #697 per-engineer release exclusion, this
  closes the auto-refuse cascade: even if the rescue cooldown
  elapses, the block metadata survives and the dispatch filter
  (`task.blocked.is_none()` in `available_dispatch_tasks`)
  excludes the task from the queue. Regression tests:
  `orphan_rescue_preserves_engineer_set_block_metadata` +
  `orphan_rescue_still_clears_block_when_task_was_not_blocked`.

## 0.11.36 — 2026-04-17

Field-report fix: after an engineer released their claim on a
parked/blocked task, the dispatcher would immediately re-queue the
same task back to the same engineer as soon as the task-level
`orphan_rescue_cooldown_secs` window opened. The task-level
exponential backoff grew correctly (5→10→20 min) but kept routing
to the releasing engineer, who would release again in 30 seconds
and burn ~10–50K LLM tokens per cycle re-reading the body before
rejecting. Observed in batty-marketing: task #555 (Thread A T+24h
harvest, owner kai-devrel) was dispatched to kai-devrel-1-1 at
06:33:51, parked with an explicit upstream-block note, released at
07:02:12, re-dispatched at 07:07:16 (5m+4s later — exactly the
base cooldown), re-released at 07:07:48.

### Fixes

- **Per-engineer release exclusion excludes the releasing engineer
  from re-dispatch** (#697) — new
  `recently_released_by: HashMap<(u32, String), Instant>` on
  `TeamDaemon`. `reconcile_active_tasks` in automation.rs populates
  it when the reconciliation reason is
  `"task no longer claimed by this engineer"` (the signal that the
  engineer cleared `claimed_by` themselves, not a transition to
  review/blocked/done). `enqueue_dispatch_candidates` filters the
  map before picking an engineer; `process_dispatch_queue` prunes
  stale queue entries covered by the exclusion. New config knob
  `board.dispatch_release_exclusion_secs` (default 3600s / 1 hour)
  outlasts the task-level exponential backoff so a different
  engineer (or manual re-route) gets the task. State persisted in
  `PersistedDaemonState.recently_released_by` as
  `{"task_id:engineer": elapsed_secs}` entries so the exclusion
  survives daemon restarts. Regression tests:
  `release_exclusion_blocks_redispatch_to_same_engineer_until_window_expires`
  + `restore_runtime_state_preserves_recently_released_by_across_restart`.

## 0.11.35 — 2026-04-17

Field-report fix: `serialize_overlapping_candidate`'s auto-persisted
overlap dependency could close a cycle when the blocking task
already depended on the candidate. Observed in batty-marketing:
task #553 (priority: high, status: in-progress) had
`depends_on: [554]` from the author, then dispatch considered #554
for dispatch, saw it overlapped files with #553, and persisted
`#554 depends_on [553]`. Auto_doctor logged
`dependency cycle detected: #553 -> #554 -> #553` every tick but
took no heal action — the tasks were only unstuck because priya
finished #554 before the cycle could gate dispatch.

### Fixes

- **Strip cycle-forming edges from overlap dependency persistence** (#696) —
  new `split_acyclic_blocking_ids` walks the on-disk `depends_on`
  graph from each blocking task and rejects any edge whose target
  (the candidate) is already reachable. Only the acyclic subset is
  handed to `append_task_dependencies`. Rejected edges are logged
  at WARN so the overlap is still visible in operator logs.
  Regressions: `split_acyclic_blocking_ids_rejects_reverse_edge`
  (direct `A depends_on B` → persisting `B depends_on A` stripped),
  `split_acyclic_blocking_ids_rejects_transitive_cycle` (3-node
  chain #A → #B → #C → persisting `C depends_on A` stripped).

## 0.11.34 — 2026-04-17

Field-report fix: tasks whose frontmatter `tags:` are thematic but
whose body prose explicitly names the owner role were still routing
to the wrong engineer. Observed in batty-marketing: task #553 with
frontmatter `tags: [content, pillar-b, x, writing]` and body line 1
"Owner: priya-writer drafts; kai-devrel schedules" was dispatched to
sam-designer-1-1 because none of the four task tags matched any
engineer's `role_name`, so `tag_overlap` scored 0 for every candidate
and routing fell through to scoring tiebreakers.

The #691 role-name seeding fix established the engineer side of the
match (each engineer's `domain_tags` contains their `role_name`).
This release adds the task side: when the body names an owner role
in prose, splice that role into the task's tag set at ranking time.
The matching engineer then scores `tag_overlap = 1`, triggers the
#692 tag-match bypass, and wins routing over non-matching peers.

### Fixes

- **Parse `Owner: <role>` from task body as a routing signal** (#695) —
  new `parse_body_owner_role` scans the task description for a line
  of the form `Owner: <lowercase-hyphen-role>` (tolerating markdown
  bullet/bold prefixes) and returns the first matching role token.
  `rank_dispatch_engineers` splices the extracted role into a local
  copy of `task.tags` before calling `rank_engineers_for_task`, so
  the synthetic tag participates in scoring without mutating the
  on-disk task file. Requires the extracted token to contain a
  hyphen, which matches the repo's role-name shape (`priya-writer`,
  `kai-devrel`, `sam-designer`, `alex-dev`) and safely rejects
  degenerate prose like `Owner: TBD`. Regression tests:
  `parse_body_owner_role_extracts_first_role_from_prose` covers
  markdown-bullet, bold-wrapped, and rejection cases;
  `dispatch_honors_explicit_body_owner_when_tags_do_not_match_role`
  reproduces the #553 scenario end-to-end.

## 0.11.33 — 2026-04-17

Field-report fix: persisted `dispatch_queue` entries carried routing
decisions across binary upgrades, silently undoing routing-bug fixes.
Observed in batty-marketing immediately after v0.11.32 deploy: task
#552 (tagged `kai-devrel`) was delivered to sam-designer-1-1 at
06:20:35Z — 34 seconds after daemon restart — because a stale
`DispatchQueueEntry { engineer: "sam-designer-1-1", task_id: 552 }`
was replayed from disk. The new binary's corrected routing logic
(#691 + #692) was bypassed because the routing decision had been
frozen at enqueue time under the old binary.

### Fixes

- **Drop `dispatch_queue` on daemon restart** (#694) —
  `restore_runtime_state` no longer assigns
  `state.dispatch_queue` into `self.dispatch_queue`. The queue
  rebuilds from current board state on the next
  `enqueue_dispatch_candidates` tick (within seconds), so we
  accept a brief re-enqueue delay in exchange for guaranteeing
  that every dispatch decision gets routed through the live
  binary's logic. Regression test:
  `restore_runtime_state_drops_dispatch_queue_across_restart`.

## 0.11.32 — 2026-04-17

Field-report fix: the SDK stall recovery loop had no upper bound on
retry attempts. When `handle_stalled_mid_turn_completion` fires, it
schedules a 30s/60s backoff for attempts 1–2 and then calls
`restart_member_with_task_context` for every subsequent attempt
indefinitely. Observed in batty-marketing: alex-dev-1-1 on task
#546 (asciinema demo) cycled stall→restart for 37+ minutes, with
the retry counter reaching attempt=6 and no progress made — each
restart burns ~200K tokens of context rebuild before immediately
stalling again.

### Fixes

- **Cap stall-mid-turn retries at 5 attempts** (#693) —
  `handle_stalled_mid_turn_completion` now enforces
  `STALLED_MID_TURN_MAX_ATTEMPTS = 5`. When the cap trips the
  daemon blocks the task with a human-triage reason, releases the
  engineer's claim (`assign_task_owners(Some(""), None)`), clears
  active-task tracking, and notifies the engineer's manager via
  `queue_message`. Subsequent stalls on the same engineer start
  from a fresh retry counter on their next assignment. Regression
  test: `stalled_mid_turn_blocks_task_after_max_attempts` verifies
  the blocked task file content, claim release, and manager inbox
  notification.

## 0.11.31 — 2026-04-17

Field-report fix: the v0.11.30 role_name tag seed worked in the
score-based ranking path, but `explain_routing_for_task` bypassed
that path whenever telemetry was mixed — a single engineer with
1–4 completions forced alphabetical fallback for the entire team,
ignoring tag_overlap entirely. Observed immediately after v0.11.30
deploy at 05:38:45: task #552 (tagged `kai-devrel`) was still
dispatched to sam-designer because alex-dev-1-1 had telemetry but
peers had zero.

### Fixes

- **Explicit tag match bypasses telemetry warmup gate** (#692) —
  `explain_routing_for_task` now additionally uses score-based
  ranking when at least one engineer's breakdown has
  `tag_matches > 0`. A task-tag → engineer-tag match is a hard
  routing signal and must take precedence over the
  "needs 5 completions per engineer" warmup requirement.
  Preserves existing warmup fallback for tasks without any tag
  match. Regression test:
  `explicit_tag_match_bypasses_telemetry_warmup_fallback`
  proves a `kai-devrel`-tagged task routes to kai-devrel-1-1
  (0 completions) over alex-dev-1-1 (2 completions, alphabetically
  prior).

## 0.11.30 — 2026-04-17

Field-report fix: role-specific task tags had no routing effect when
engineers lacked completion history. Observed in batty-marketing:
task #550 tagged `kai-devrel` + `engagement` + `x-twitter` +
`star-conversion` was dispatched to sam-designer (alphabetically
first idle engineer), who released it within seconds because the
task belonged to kai-devrel's role. The fresh engineer profiles had
empty `domain_tags` — so `tag_overlap` scored 0 for every candidate
and ranking fell back to alphabetical order.

### Fixes

- **Seed `domain_tags` with `role_name` at dispatch time** (#691) —
  `enqueue_dispatch_candidates` and `process_dispatch_queue` now
  insert each engineer's `role_name` into their `EngineerProfile
  .domain_tags` after `load_engineer_profiles` returns. This means
  a task tagged with a role name (`kai-devrel`, `sam-designer`,
  etc.) contributes a matching `tag_overlap` score *before* any
  completion history exists, correctly preferring the role-matching
  engineer over alphabetical fallback. Seeding is additive — does
  not replace the completion-history tags already extended by
  `apply_completed_profile`. Regression test:
  `dispatch_queue_seeds_role_name_into_domain_tags_for_tag_routing`
  verifies a `kai-devrel`-tagged task routes to `kai-devrel-1-1`
  over `alex-dev-1-1` and `sam-designer-1-1` peers.

## 0.11.29 — 2026-04-17

Field-report fix: the v0.11.23 guard that skipped zero-output
restarts for non-Working agents was not sufficient. A shim that sat
Idle for 18 minutes, then received an inbox message and transitioned
to Working for 8 seconds, was still killed because the existing
check tested only *current* state — not *how long* the agent had
been continuously Working.

### Fixes

- **`working_since` gate on zero-output cold respawn** (#690) — new
  `working_since: HashMap<String, Instant>` on `TeamDaemon`.
  Populated on Idle→Working transitions in `handle_shim_event`,
  cleared on Working→Idle and on cold respawn.
  `handle_context_pressure_stats` now requires
  `working_since.elapsed() >= ZERO_OUTPUT_THRESHOLD_SECS` (600s) in
  addition to the existing `is_working` check before cold-respawning
  a lifetime-zero-output shim. Prevents the 2026-04-17 04:50:02
  pattern where priya-writer-1-1 (uptime 1100s, state Working for
  8s, zero lifetime output) was cold-respawned immediately after a
  jordan-pm inbox message arrived — wasting the fresh shim context
  and burning another startup round. Regression tests:
  `zero_output_gate_skips_fresh_working_transition`,
  `zero_output_gate_fires_after_sustained_working_with_no_output`,
  `state_change_to_working_seeds_working_since_and_idle_clears_it`.

## 0.11.28 — 2026-04-17

Field-report fix: the v0.11.24 "exponential orphan-rescue backoff"
never actually grew in production — every rescue reset the counter to
1, so cascades kept re-dispatching at the base 5-minute cadence.
Three root-cause fixes.

### Fixes

- **Cascade-observation window replaces dispatch-gate window for
  count growth** (#689) — the dispatch cooldown gates *dispatch*, so
  the next rescue for a cascading task always fires *after*
  `effective_cooldown` has elapsed (when the rescued task gets
  re-dispatched and the new claimer quickly releases). The old
  `is_active`-gated growth check in `record_task_rescue` therefore
  never triggered: `elapsed` was always ≥ `effective_cooldown`, so
  count reset to 1 on every rescue. Growth now checks a wider
  cascade-observation window (2× effective cooldown) which succeeds
  across real-world gaps like "dispatch opens at 5m, engineer
  releases at 5m+15s → rescue fires, 5m+15s < 10m → count grows."
  Observed on task #519 in batty_marketing: the cascade
  re-dispatched every ~5 minutes (03:32:54 → 03:37:56 → 03:38:09 →
  03:43:11 → 03:43:29 → 03:48:36 → 04:18:53 → 04:19:06) with count
  pinned at 1. New `RescueRecord::cascade_window` +
  `in_cascade_window` helpers distinguish the dispatch gate
  (`dispatch_blocked`) from the cascade-membership check.

- **Pre-queued dispatch entries now re-check the rescue cooldown at
  drain time** (#689) — an entry can be queued by
  `enqueue_dispatch_candidates` and then the same tick's
  `rescue_orphaned_tasks` fires a rescue for that task. The
  already-queued entry would then bypass the freshly-set cooldown
  and dispatch on the very next drain.
  `process_dispatch_queue` now prunes queued entries whose task
  entered the rescue cooldown after queuing.

- **`recently_rescued_tasks` survives daemon restarts** (#689) — the
  rescue-record map was in-memory only, so any daemon restart or
  hot-reload wiped the exponential-backoff counter and resumed the
  cascade at count=1 / 5-min cooldown. New
  `PersistedRescueRecord { last_rescued_elapsed_secs, count }` rides
  on `PersistedDaemonState` with `#[serde(default)]`; restore
  reconstructs the `Instant` via `Instant::now() - elapsed`,
  matching the existing nudge / planning-cycle persistence pattern.
  Retention also moved from the dispatch-gate window (`is_active`)
  to the full cascade window (`in_cascade_window`) so records live
  long enough for the next rescue to find them.

### Tests

- `record_task_rescue_grows_count_across_dispatch_gate_openings`
  regression — simulates a rescue at 150s elapsed (past the 100s
  dispatch gate but inside the 200s cascade window) and asserts
  count grows to 2, then a rescue at 500s elapsed (past cascade
  window) resets to 1.
- `restore_runtime_state_preserves_recently_rescued_tasks_across_restart`
  regression — persists a count=4 record with `last_rescued` 180s
  ago, restores, asserts count=4 and elapsed ~180s on the restored
  `Instant`.

## 0.11.27 — 2026-04-17

Tenth-round field-report fix: slow empty cycles no longer re-fire on completion.

### Fixes

- **Cooldown anchor slides forward on empty cycles** (#688) — the
  planning cooldown is measured from `planning_cycle_last_fired`, so a
  10-minute empty cycle hits the 2× backoff boundary (600s for
  consecutive_empty=1) exactly when it completes. The next tick sees
  `elapsed >= cooldown` and fires a fresh cycle within the same tick.
  Observed at 04:13:35 in batty_marketing: response applied →
  consecutive_empty=1 → new planning cycle triggered 181ms later.
  Empty-cycle paths now bump `last_fired` to `Instant::now()` on
  completion so the next cycle gates on time-since-completion, not
  time-since-fire. Productive cycles (created > 0) leave `last_fired`
  alone so the architect can keep planning as fast as the pipeline
  needs it.

## 0.11.26 — 2026-04-17

Ninth-round field-report fix: persist planning-cycle state immediately
on fire, not just at heartbeat.

### Fixes

- **Planning cycle fires twice on quick restart** (#687 followup) —
  v0.11.25 persisted `planning_cycle_last_fired` in the 5-min heartbeat
  path, but observed double-fire at 04:03:22 + 04:03:29 when the daemon
  restarted 6 seconds after firing the first cycle (hot-reload or fast
  manual restart). The in-memory `last_fired` update had not yet been
  checkpointed, so the restored state still showed `None` and the new
  daemon fired a second planning cycle at the architect. Now
  `tact_check` persists immediately after setting `last_fired` so the
  checkpoint is durable before any plausible restart window.

## 0.11.25 — 2026-04-17

Eighth-round field-report fix: architect planning cadence survives restart.

### Fixes

- **Planning-cycle backoff persists across daemon restart** (#687) —
  `planning_cycle_last_fired` and `planning_cycle_consecutive_empty`
  lived only in-memory on `TeamDaemon`, so a daemon that had learned
  the board was stuck (and backed off to 6× cadence via #681) reset
  both to zero on every restart. A fresh daemon then fired an architect
  planning cycle ~6 seconds after startup on the same stuck board —
  observed in `batty_marketing` at 03:56:52, 6 seconds after daemon
  start. Both fields now round-trip through `PersistedDaemonState`
  (`planning_cycle_last_fired_elapsed_secs: Option<u64>` +
  `planning_cycle_consecutive_empty: u32`). `restore_runtime_state`
  reconstructs the `Instant` by subtracting the saved elapsed from
  `Instant::now()`, matching the pattern used for nudge idle state.
  Regression test (`restore_runtime_state_preserves_planning_cycle_backoff_across_restart`)
  covers the end-to-end restore path with a 120s-old, 4-empty state.

## 0.11.24 — 2026-04-17

Seventh-round field-report fix: stop the cascade that resumes the moment
the rescue cooldown expires.

### Fixes

- **Exponential-backoff orphan-rescue cooldown** (#686) — v0.11.22 added
  a 5-minute dispatch cooldown after an orphan rescue, but observation
  in `batty_marketing` showed the cascade simply resumed once the window
  expired: 03:32:54 rescue → 03:37:56 dispatch to sam (5m 2s later) →
  03:38:09 sam released (13s of "work") → 03:43:11 dispatch to alex →
  03:43:29 alex released (18s of "work") → next rescue cycle, et cetera.
  Each bounce burns ~200K tokens on a turn the engineer immediately
  rejects. `recently_rescued_tasks` now carries a `RescueRecord` with a
  `count` field; repeated rescues of the same task within the current
  cooldown widen the effective window to `orphan_rescue_cooldown_secs *
  2^min(count-1, 4)` (1×, 2×, 4×, 8×, 16× cap). With the default 300s
  base, that's 5 / 10 / 20 / 40 / 80 minutes — enough total quiet time
  (155 min after four rescues) for a human or an architect to notice
  and re-route rather than letting the dispatch loop churn through
  every idle peer. The pruning logic in `enqueue_dispatch_candidates`
  and `rescued_task_ids` both consult the per-task effective cooldown
  instead of the base value.

## 0.11.23 — 2026-04-17

Sixth-round field-report fix: stop tearing down idle agents.

### Fixes

- **Zero-output restart gated on `Working` state** (#685) — the context
  health check restarted any agent whose shim reported zero output for
  10+ minutes, regardless of `MemberState`. Idle agents with empty
  inboxes and no active task legitimately produce no output, so the
  handler was tearing them down every 10 minutes and paying a fresh
  shim cold-respawn cost with no behavioral gain. Observed in
  `batty_marketing`: 5 agents (kai, priya, sam, alex, jordan) all
  force-restarted at uptime 600s in a single tick. The zero-output
  branch now checks `is_working` first and returns early when the
  member is Idle. Working members whose shim hung still restart as
  before.

## 0.11.22 — 2026-04-17

Fifth-round field-report fix: damp the release→redispatch loop.

### Fixes

- **Orphan-rescue cooldown before auto-redispatch** (#684) — when an
  engineer releases a parked/in-progress task (by clearing their own
  claim) and the daemon reconciles the board back to `todo`, the
  dispatch queue previously re-handed the task to the first idle peer
  on the same tick — which then rejected it, triggering another rescue
  → re-dispatch cycle. Observed in `batty_marketing`: kai-devrel
  released #519 at 03:06:48, sam-designer got it at 03:06:48, released,
  alex-dev got it at 03:07:16. Introduces
  `board.orphan_rescue_cooldown_secs` (default 300s / 5 min) and a
  `recently_rescued_tasks` map on `TeamDaemon`. Both the runtime
  orphan-rescue path (`automation.rs`) and the auto-doctor reset path
  (`auto_doctor.rs`) insert into the map; `available_dispatch_tasks`
  filters on it. Gives the releasing engineer or the manager a 5-minute
  window to reclaim/re-route before auto-dispatch takes over. Two
  regression tests in `src/team/dispatch/queue.rs::tests`.

## 0.11.21 — 2026-04-17

Fourth-round field-report fix: preserve valid in-progress claims across
daemon restarts.

### Fixes

- **Auto-doctor re-attaches valid claims after hot-reload** (#683) — on
  daemon restart (or any hot-reload) `active_tasks` is cleared, so every
  in-progress task momentarily looks "orphaned" to the auto-doctor. The
  previous behavior was to reset the claim and bounce the task back to
  `todo`, which immediately re-dispatched it to another engineer on the
  next tick — wasting the original engineer's context and, in the
  observed `batty_marketing` cascade, escalating alex-dev-1-1 to the
  1M-token context tier. Now when the claim is held by a valid engineer
  role, the daemon re-attaches the task to that engineer's
  `active_tasks` slot instead of resetting. Unknown-engineer claims
  still reset to todo as before. New auto-doctor action type
  `orphaned_in_progress_reattached`; tests in
  `src/team/daemon/health/auto_doctor.rs::tests`.

## 0.11.20 — 2026-04-17

Third-round field-report fix: stop wrong-role dispatch from ballooning
engineer context.

### Fixes

- **Respect `assignee:` frontmatter in dispatch** (#682) — tasks whose
  `assignee:` names a non-engineer member (manager/architect/writer) are
  now filtered out of the dispatch pool entirely; they're messages for
  that member's inbox, not work for the engineer queue. When `assignee:`
  names an engineer, dispatch routes only to that engineer (waiting if
  they're busy rather than re-routing to a peer). Observed in
  `batty_marketing`: kai-devrel ballooned to 170 % context (1.7 M
  tokens) because the same non-engineer-assigned tasks were dispatched,
  rejected, and re-dispatched on every tick. Adds
  `assignee: Option<String>` to `Task` + `Frontmatter`, a
  `non_engineer_member_names()` helper on the daemon, and an extra
  filter in `available_dispatch_tasks` + `rank_dispatch_engineers`.

## 0.11.19 — 2026-04-17

Second round of field-report fixes from the `batty_marketing` production
run. Focus: stop burning orchestrator tokens on stuck boards, and
unblock dependents of archived tasks.

### Fixes

- **Planning-cycle empty-response backoff** (#681) — when the architect
  returns zero new tasks from a planning cycle (typical on a fully
  blocked board where every task has `blocked: true`), the effective
  cooldown now grows linearly: 1x → 6x of
  `workflow_policy.planning_cycle_cooldown_secs`. A 5-minute base
  settles to a 30-minute check on a stuck board, then snaps back to 1x
  as soon as a cycle produces any tasks. Prevents the observed
  "planning ping storm" in `batty_marketing` where 12+ consecutive
  empty cycles each burned a multi-hundred-thousand-token architect
  call. New field `planning_cycle_consecutive_empty: u32` on
  `TeamDaemon`; logic in `tact_check` + `apply_planning_cycle_response`.
- **Archived deps unblock dependents** (batty side of #680) — dispatch
  and allocation dep-resolution now treat `status: archived` as
  equivalent to `status: done`. Previously, an archived dependency
  (common after long-running projects wind down and tasks are
  archived in-place rather than moved) left downstream tasks stuck
  in the dispatch filter forever. New `dep_status_satisfied(status)`
  helper in `src/team/dispatch/queue.rs`; parallel fix in
  `src/team/allocation.rs`.

## 0.11.18 — 2026-04-16

Field-report fixes surfaced by a real `batty_marketing` production run.
Three surgical, backward-compatible defect fixes. No behavior changes
for default configurations — only corrects pathological cases.

### Fixes

- **Engineers no longer auto-claim planning / design / content tasks**
  (#677) — new `board.dispatch_excluded_tags` config (default empty)
  holds a case-insensitive tag list; any task with a matching tag is
  skipped by the dispatch queue and must be claimed explicitly. Typical
  production value: `["planning", "design", "content", "ops"]`. Prevents
  the engineer pool from stealing work intended for non-engineering
  roles. Implementation in `src/team/dispatch/queue.rs`; both
  `next_dispatch_task` and `enqueue_dispatch_candidates` honor the
  filter.
- **Escalation ping-storm suppression** (#678) — `record_task_escalated`
  now dedupes on `(role, task_id, reason)` through the existing
  `recent_escalations` cache with a 10-minute cooldown. Watchdogs that
  re-examine the same signal each tick (stall, poll_shim, owned_tasks,
  merge, automation) no longer produce duplicate `task_escalated`
  telemetry for the same underlying state, so managers see one alert
  per distinct cause instead of a flood during planning cycles.
- **Aging-alert framing for non-git / multi-repo projects** (#679) —
  `maybe_emit_task_aging_alerts` now branches on
  `self.is_git_repo && !self.is_multi_repo`. When the project root is
  not a single git repo, the checkpoint request, escalation reason,
  and manager notification all reframe away from "commits ahead of
  `main`" to "no progress signals during the cooldown window", so
  non-code teams (marketing, design, research) don't see spurious
  branch advice.

### Notes

- **#680 (kanban-md `edit --release` blocked by archived deps)** is
  upstream in `antopolskiy/kanban-md`, not in the batty repo; the batty
  daemon consumes the tool but doesn't own its dependency-validation
  logic. Workaround while waiting for an upstream fix:
  `kanban-md edit <task-id> --remove-dep <archived-dep> --claim`.

## 0.11.17 — 2026-04-16

Closes the five outstanding `good first issue` items. No behavior
regressions — each item either adds discoverability (error hints,
docs, a log flag) or a new scaffold option.

### Features

- **Python team template** (#8) — `batty init --template python`
  scaffolds a 5-pane team with a Python-native engineer prompt that
  drives `pytest` / `ruff` / virtualenv activation. `InitTemplate`
  grows a `Python` variant wired through `src/cli.rs`,
  `src/main.rs`, and `src/team/init.rs`; new templates land at
  `src/team/templates/team_python.yaml` and `python_engineer.md`.
- **`--quiet` flag on `batty start`** (#10) — suppresses the
  launch-time "detaching…" / "attached" banner when running under
  supervisors or CI. Existing callers are unaffected; default is
  `false`.

### Fixes

- **Friendlier error when tmux is not installed** (#7) — `tmux.rs`
  maps `io::ErrorKind::NotFound` to the new
  `TmuxError::NotInstalled` variant, whose `Display` impl prints a
  one-line install hint (brew / apt / dnf) instead of the default
  "No such file or directory".
- **`BATTY_LOG` env var for log filtering** (#9) — `main.rs`
  prefers `BATTY_LOG` over `RUST_LOG`, giving Batty-native users a
  discoverable knob without losing the standard Rust fallback.

### Docs

- **Environment variables reference** (#11) — new
  `docs/reference/environment-variables.md` consolidates every
  `BATTY_*` knob in one table with defaults and examples. Linked
  from the main navigation under Reference.

## 0.11.16 — 2026-04-16

Field-reliability pass from a long `~/nether_earth_remake` run:
non-recoverable auth failures, logspam, and lingering orphan claims
now heal on their own instead of waiting for human intervention.

### Fixes

- **Codex refresh-token failure now parks the backend instead of
  looping** — `shim/runtime_codex.rs` adds `detect_auth_required()`,
  matching `turn.failed` / `error` payloads that mention a reused
  refresh token or a "log out and sign in again" hint. The shim
  emits a new `Event::AuthRequired`, which flips
  `BackendHealth::AuthRequired` and gates the daemon's crash-respawn
  path via `member_backend_parked()`. Previously a single bad
  refresh token could spawn 140+ retries in a few minutes; now the
  shim halts cleanly and the operator is notified.
- **Context-bump warnings deduplicated in the SDK shim**
  (`shim/runtime_sdk.rs`) — the 1M-context bump log now fires once
  per process at `warn!`; subsequent bumps drop to `debug!`. A
  single shim session had been emitting 68 identical WARN lines in
  production logs; this cuts the noise without losing the first
  signal.
- **Orphaned in-progress tasks are reclaimed on daemon startup**
  (`team/daemon/poll.rs`) — `auto_doctor_reset_orphaned_in_progress`
  now runs immediately after state restoration instead of only
  every 10 poll cycles. Prevents tasks from staying "overdue" for
  an hour or more after a daemon restart when the active-task map
  has been cleared.

## 0.11.15 — 2026-04-16

Tiered inbox control plane lands behind a feature flag, turning the
0.11.14 design doc into a working implementation.

### Features

- **Tiered inbox queues implementation** (#658) — new
  `src/team/inbox_tiered.rs` module adds a 4-tier Maildir layout
  (priority/work/content/telemetry) with per-tier TTLs
  (1h/30m/15m/5m by default). Gated by `workflow_policy.tiered_inboxes`
  (default `false`), so the change is fully additive and non-breaking.
  Write path uses `deliver_flag_aware` at 5 production call sites in
  `delivery/routing.rs`; supervisor reads use `pending_messages_union`
  in `daemon/interventions/mod.rs` and `supervisory_notice.rs` so both
  layouts are safe to read regardless of flag state. Daemon tick
  `maybe_sweep_tiered_inboxes` (60s cooldown, no-op when flag off)
  expires per-tier backlog. Adds `queue_tier()` to `MessageCategory`
  and 18 targeted tests; full library suite (3517 tests) stays green.
  Follow-up work (tiered digest formatting, per-queue rate limits) is
  deferred until real-world per-tier volume is observable.

## 0.11.14 — 2026-04-15

Inbox control-plane design doc and worktree hygiene pass.

### Design

- **Tiered inbox queue design** (#658) — comprehensive design document
  (`planning/inbox-control-plane-design.md`) proposing 4-tier message
  queues (priority/work/content/telemetry) with per-tier TTLs, write-time
  classification, and a 3-phase backwards-compatible migration path.
  Includes catalog of all inbox message types with source-file references
  and 5 follow-up implementation tickets.

### Maintenance

- **Worktree and stash hygiene** (#673) — pruned 17 stale worktree
  entries, removed 5 abandoned worktrees with their branches, cleared 133
  accumulated stashes, deleted 7 stale remote-tracking branches, and
  removed 1 stale release worktree. Board cleaned of all resolved tasks.

## 0.11.13 — 2026-04-15

Binary freshness detection surfaces stale daemon binaries; read-only
telemetry DB opener prevents `batty status` from blocking on daemon
write locks.

### Features

- **Detect stale daemon binary vs main HEAD** (#675) — new
  `binary_freshness` health module compares the running binary's mtime
  against `git log HEAD -- src` commits. Reports "Daemon Binary: STALE —
  N commits behind main" in `batty status` and emits
  `daemon_binary_stale` events. 10-minute threshold avoids false alarms;
  docs-only commits are filtered out. Hourly recheck via daemon tick.

### Fixes

- **`batty status` no longer blocks on daemon write locks** (#676) — CLI
  status path now opens the telemetry SQLite DB in read-only mode
  (`open_readonly`) with a 2-second busy timeout, skipping schema init.
  The daemon's own connection gets a 5-second `busy_timeout` PRAGMA.
  ETA estimation also uses the read-only opener.

## 0.11.12 — 2026-04-15

Unwedge Working engineers stuck on stale branches with dirty worktrees;
board hygiene pass closes stale/resolved stability tickets.

### Fixes

- **Branch-recovery-blocked self-heal for Working engineers** (#666) —
  `maybe_align_engineer_worktree_with_task` no longer gates on
  `MemberState::Idle`. The self-heal path (auto-commit WIP on the stale
  branch, then checkout the authoritative lane) now runs for Working
  members too, so lanes stop getting wedged on `"automatic branch recovery
  blocked: dirty worktree"` when the engineer happens to be mid-session.
  The auto-save commit is non-destructive — the stale branch retains every
  byte of user work — and the upstream `audit_due` cooldown still
  rate-limits how often the path runs. New regression test:
  `reconcile_active_tasks_self_heals_working_engineer_on_stale_branch`.

### Board hygiene

Closed stale or already-implemented tickets uncovered during the 2026-04-15
board-drain pass:

- #629 (auto-repair legacy telemetry schemas) — already shipped via
  `SchemaColumn`-driven column-aware `repair_legacy_schema` in
  `src/team/telemetry_db.rs`; stuck only on a stale automation-injected
  block_reason from an unrelated worktree preservation incident that
  #659 has since addressed.
- #649 (suppress manager patch attempts on /tmp) — no production code
  path produces `/tmp/nether-earth-task*` paths; integration worktrees
  live under `.batty/integration-worktrees/` exclusively and
  `worktree_path` frontmatter is project-root-relative. The reported
  logs are manager-agent internal (codex session hallucination), not a
  batty source bug.
- #668 (proactive context-exhaustion handoff) — already fully
  implemented: `Event::ContextApproaching` from the shim triggers
  `handle_context_pressure_warning(_, _, _, 80)` which calls
  `preserve_handoff` before context_handoff_enabled restart.
- #669 (tact_check couples task identity to title-derived filenames) —
  `tact_check` uses `load_tasks_from_dir` (ID-keyed); title references
  in `src/team/tact/parser.rs` are for generated-task dedup, not file
  lookup. Stale.
- #658 (agent-mission inbox control-plane design) — unblocked from a
  stale verification-failure block_reason (those tests pass on current
  main); demoted to backlog/medium because the tactical fix in #650 has
  resolved the operational urgency.

## 0.11.11 — 2026-04-15

Preserve unmerged engineer commits across every destructive branch reset.

### Fixes

- **Dispatch reconciliation must preserve commits ahead of main** (#659)
  — every `git checkout -B <branch> <base>` that would rewrite a branch
  ref now first archives any commits the branch carried ahead of its
  target to a `preserved/<slug>-<timestamp>` backup branch. Covers the
  three reset paths that previously discarded unmerged work:
  - `reset_worktree_to_base_with_options_for` — archives `base_branch`
    before recreating it from `main`. Supplements the existing
    `PreservedBeforeReset` HEAD archive (which still runs when
    `current_branch == base_branch`) by also covering stale commits
    from prior sessions.
  - `reset_worktree_to_base_if_clean` — archives `base_branch` before
    the background clean-reset path so health-check reconciliation no
    longer drops completed-but-unmerged task work.
  - `ensure_worktree_branch_for_dispatch` — archives the `expected_branch`
    before overwriting it from the dispatch start ref, so dispatches that
    re-point a branch to `main`/`origin/main` keep whatever the branch
    already had.

  The helper (`archive_branch_if_commits_ahead`) is a no-op when the
  branch is missing or has zero commits ahead of the reset target, uses
  a unique suffix when a timestamp collides with an existing archive, and
  logs commit count + archive name on success. (`src/worktree.rs`)

## 0.11.10 — 2026-04-15

Quota-retry gating across the four daemon subsystems that misread a
parked engineer as "not producing output."

### Fixes

- **Backend-health state no longer flaps quota_exhausted→healthy**
  (#674 defect 1) — when a shim emits `Event::QuotaBlocked` with a
  `retry_at_epoch_secs` deadline, the daemon now records the deadline
  in a new `backend_quota_retry_at` map. The periodic
  `check_backend_health` probe refuses to transition a member out of
  `QuotaExhausted` while the deadline is in the future, even if
  `which <agent>` succeeds. A successful poll_shim ping is **not**
  evidence of quota recovery — only the elapsed deadline or an
  operator reset (daemon restart / bench) can clear the state.
  (`src/team/daemon/health/checks.rs`,
  `src/team/daemon/health/poll_shim.rs`, `src/team/daemon.rs`)
- **Dispatch selection skips quota-parked engineers** (#674 defect 2)
  — `rank_dispatch_engineers` now consults `member_backend_parked`
  before handing a task to an idle engineer. This breaks the
  15-minute stall-timer rotation cascade that redispatched each
  reclaimed task to another quota-blocked engineer.
  (`src/team/dispatch/queue.rs`)
- **Stall-timer reclaim refuses to reclaim parked engineers** (#674
  defect 3) — `maybe_manage_task_claim_ttls` short-circuits when the
  claimed engineer is backend-parked, so tasks stay put until the
  quota window elapses instead of rotating to the next quota-blocked
  engineer. (`src/team/daemon/automation.rs`)
- **Stuck-task escalator honors backend-parked state** (#674 defect
  4) — `maybe_emit_task_aging_alerts` now skips the
  `task_stale` / `task_escalated` path (and clears pending aging
  cooldowns) when the owner is backend-parked. Parked engineers are
  waiting, not stalled; escalating them during an outage produced
  recursive stuck-task escalations against the very ticket meant to
  fix the bug. (`src/team/daemon/automation.rs`)
- **`member_backend_parked` helper** — a single `TeamDaemon` method
  returns true whenever either the cached `QuotaExhausted` state or
  the tracked `retry_at` deadline indicates the member cannot make
  progress. Callers across the daemon use it to gate any subsystem
  that interprets "engineer not producing output" as a problem.
  (`src/team/daemon.rs`)

## 0.11.9 — 2026-04-15

Daemon-exit classification so the watchdog stops restarting on hard failures.

### Fixes

- **Watchdog circuit-breaks immediately on unrecoverable exits** (#665)
  — `record_watchdog_crash` now takes a `DaemonExitObservation` with a
  classified `exit_category`. Tmux deaths, missing team config, and
  other pre-flight failures trip the breaker on the first occurrence
  instead of after N restart attempts. The watchdog also reads the
  most recent `daemon_exit` event to learn *why* the daemon died and
  threads that through to status output and telemetry.
  (`src/team/daemon_mgmt.rs`, `src/team/events.rs`, `src/team/status.rs`,
  `src/team/daemon/poll.rs`, `src/team/daemon/telemetry.rs`, `src/tmux.rs`)
- **`batty status` surfaces the last daemon exit reason** — when the
  daemon is stopped, the watchdog health line now includes the exit
  category and the human-readable reason so operators don't have to
  grep daemon.log to find out what crashed.
  (`src/team/status.rs`)

## 0.11.8 — 2026-04-15

Worktree-mutation safety and nightly fuzz CI fix.

### Fixes

- **Daemon no longer mutates dirty engineer worktrees** — the dispatch
  queue's auto-recovery path now refuses to touch worktrees that still
  have user changes, re-queuing them with an actionable preservation
  blocker instead of silently staging files. During the 2026-04-14 codex
  quota incident the old `preserve_failed_reset_skipped` loop was
  generating phantom staged changes on idle engineers every tick.
  (`src/team/dispatch/queue.rs`, `src/worktree.rs`, `src/team/task_loop.rs`)
- **Worktree-mutation audit log** — every `checkout -B` / reset
  operation now logs the cwd, subsystem tag, and user-change paths so
  unexpected mutations can be traced back to their triggering
  subsystem.
  (`src/worktree.rs`, `src/team/task_loop.rs`)
- **Nightly scenario-framework fuzz job compiles** — `cargo test` only
  accepts one positional test-name filter, so the three-name single
  invocation in `.github/workflows/ci.yml` was failing with
  `unexpected argument 'fuzz_workflow_with_faults'`. Each filter now
  runs in its own invocation under the same PROPTEST budget.
  (`.github/workflows/ci.yml`)

## 0.11.7 — 2026-04-15

Shim quota handling, CI stability, and dispatch-pipeline polish.

### Fixes

- **Codex shim no longer tight-loops on `usage limit` errors** — the
  Codex shim now parses `"try again at ..."` timestamps, records a
  `quota_blocked_until` deadline on `CodexState`, drains queued
  messages with an error, emits one `Event::QuotaBlocked`, and refuses
  new sends until the deadline passes. The daemon's `poll_shim` handler
  marks the backend `QuotaExhausted` and raises a single orchestrator
  action instead of respawning the `codex exec` process every second.
  (`src/shim/protocol.rs`, `src/shim/runtime_codex.rs`,
  `src/team/daemon/health/poll_shim.rs`)
- **Main-smoke summaries survive `CARGO_TERM_COLOR=always`** — cargo's
  ANSI escapes no longer slip past the summary filter in CI, which was
  reporting "`    Checking ...`" as the failure line on Ubuntu runners.
  (`src/team/daemon.rs`)
- **Preflight tests share the PATH mutex with the rest of the suite** —
  `health::test_helpers::PATH_LOCK` is now a re-export of the one in
  `team::test_support`, fixing intermittent
  `startup_preflight_accepts_available_agent_binaries` failures when
  other suites mutated `PATH` in parallel.
  (`src/team/daemon/health/mod.rs`)
- **Clippy `-D warnings` gate passes again** — two crate-internal
  helpers with 8 arguments now carry `#[allow(clippy::too_many_arguments)]`
  and a pair of duplicate supervisory-pressure branches are collapsed.
  (`src/team/checkpoint.rs`, `src/team/task_loop.rs`,
  `src/team/supervisory_notice.rs`)
- **`cargo fmt` and `mdformat` gates green** — a stray blank line in
  `src/team/delivery/mod.rs` doc comments and list-indentation drift in
  `docs/orchestrator.md` have been cleaned up.

### Dispatch & review pipeline

Numerous dispatch, stall detection, auto-merge, and worktree-hygiene
refinements landed in parallel with the fixes above; see the commit log
between `v0.11.6..v0.11.7` for the full list.

## 0.11.6 — 2026-04-14

Dispatch stability fixes.

### Fixes

- **Recover dispatch when engineer base branch stays ahead** — when an
  engineer's base branch is ahead of main (from an unpushed commit that
  won't rebase), the dispatch queue now recovers instead of stalling.
  (`src/team/dispatch/queue.rs`)
- **Fix task branch ownership drift in verification and dispatch** —
  addresses inconsistencies where a task's branch no longer matches the
  engineer's expected branch, causing verification and dispatch to reject
  valid work. (`src/team/daemon/verification.rs`, `src/team/dispatch/mod.rs`,
  `src/team/task_cmd.rs`)

## 0.11.5 — 2026-04-13

Critical dispatch pipeline fix.

### Fixes

- **Engineers no longer stall in Working state with no active task** —
  When `mark_member_working()` fires but task delivery subsequently fails,
  the engineer gets stuck in Working with no active_tasks entry, blocking
  all future dispatch. The dispatch queue now detects this inconsistency
  and force-transitions the engineer to Idle before the delivery gate.
  (`src/team/dispatch/queue.rs`)

## 0.11.4 — 2026-04-13

Critical stability fix for the auto-merge review pipeline.

### Fixes

- **Config-only changes no longer stall the merge pipeline** —
  `has_config_changes` was a hard blocker in `evaluate_auto_merge_candidate`,
  routing any commit touching `.json`/`.yaml`/`.toml` files to manual review.
  The manager agent review path is unreliable with codex agents, causing tasks
  to pile up in review indefinitely. Config changes now reduce confidence
  (-0.15) but no longer add a hard-blocking reason. If combined with other
  risk factors that push confidence below threshold, the confidence gate still
  catches it. (`src/team/auto_merge.rs`)
- **Generated data files excluded from config detection** —
  `is_config_file()` now excludes paths under `generated/`, `reference/`,
  `fixtures/`, `tests/`, and lockfiles. These are outputs, not configuration.
  (`src/team/auto_merge.rs`)

## 0.11.3 — 2026-04-11

Patch release for four production regressions found during live monitoring
immediately after 0.11.2.

### Fixes

- **Daemon startup no longer crash-loops on non-git content teams** —
  preflight now skips `ensure_git_ready` and
  `ensure_worktree_operations` when no engineer is configured with
  `use_worktrees = true`. This keeps marketing/docs/ops teams running
  from plain directories instead of failing at startup on an unnecessary
  git prerequisite. (`src/team/daemon/health/preflight.rs`)
- **1M-context agents stop tripping false context-pressure restarts** —
  proactive pressure checks now bump the effective context window to the
  1M tier when observed token usage already exceeds the stripped SDK
  model name's nominal 200K budget. (`src/shim/runtime_sdk.rs`)
- **Cache-creation tokens are counted once instead of twice** —
  `SdkOutput::usage_total_tokens` now delegates to the canonical
  `token_usage().total_tokens()` path so
  `cache_creation_input_tokens` is de-duped against the classified
  ephemeral cache buckets instead of being summed on top of them.
  (`src/shim/sdk_types.rs`)
- **Discord task assignment previews render as plain text again** —
  `task_assigned` bodies no longer ship as literal `spoiler` markup.
  The preview now stays readable in-channel and truncates with an
  explicit expand-style marker near Discord's embed description limit.
  (`src/team/discord_bridge.rs`)
- **Auto-resolved rebases stay non-interactive in merge automation** —
  batty now runs git with `GIT_EDITOR=true`, so a resolved
  `git rebase --continue` does not fail trying to open `vi` while
  finalizing the rebased commit message in headless runs.
  (`src/team/merge/git_ops.rs`)

## 0.11.2 — 2026-04-11

Emergency stability follow-up to 0.11.1. Fixes the documented "daemon
event loop freezes after 10-15 min productive window" pattern that has
been the #1 reliability issue for weeks of live-agent monitoring.

### Fixes

- **Daemon event loop freeze after 10-15 min productive window** —
  The parent-side `Channel` to each shim subprocess had a read timeout
  (25ms) but **no write timeout**. `Channel::send` called
  `stream.write_all()`, which on Unix stream sockets blocks
  indefinitely when the peer stops draining its receive buffer. Under
  normal operation shims read commands as fast as they arrive, so the
  block never materialises — but when a shim wedges (slow codex tool
  call, hung SDK stream, blocked subprocess), its receive buffer fills
  up and the daemon's next `send_ping` / `send_kill` / `Resize` /
  message delivery blocks inside `write_all` waiting for bytes that
  never drain. The `ping_pong` health subsystem runs inside the main
  poll loop, so one wedged shim freezes the entire event loop: no
  more merges, no more dispatch, no more logging. Restart buys another
  10-15 min productive window before the cycle repeats. New
  `Channel::set_write_timeout` helper mirrors `set_read_timeout`;
  `shim_spawn::spawn_shim` applies a 2-second ceiling so a wedged
  shim surfaces as a send error within one or two ping_pong cycles
  and flows through the usual stale-handle / respawn path instead of
  hanging forever. Regression test
  `shim::protocol::tests::send_times_out_when_peer_stops_reading`
  opens a socketpair, sets a 50ms write timeout, and blasts large
  payloads without ever draining the peer — asserts `send()` returns
  a WouldBlock/TimedOut error within 5s instead of hanging.
  (`src/shim/protocol.rs`, `src/team/daemon/shim_spawn.rs`)

## 0.11.1 — 2026-04-11

Stability patch release surfaced by a live-daemon monitoring session on
top of 0.11.0. Fixes one silent throughput killer in the auto-merge
path, one planner crash triggered by newer kanban-md output, and
unblocks main CI after a macOS runner flake.

### Fixes

- **Auto-merge silently dropped every task** (`missing_packet`) —
  `handle_engineer_completion` moved a passing task to review and
  enqueued a merge request but never wrote the `branch`, `commit`,
  `worktree_path`, `tests_run`, or `tests_passed` markers to the task's
  workflow metadata. The merge queue's
  `missing_completion_packet_detail` then rejected every single
  request with `branch marker missing; commit marker missing; worktree
  marker missing`, preventing any auto-merge from ever landing. Tasks
  piled up in `review` indefinitely under multi-engineer load. The
  test fixture had been masking the bug by pre-seeding the metadata;
  that pre-seed is now removed so the existing `completion_*` tests
  exercise the real production path end-to-end, and a new
  `handle_engineer_completion_records_packet_metadata_for_auto_merge`
  regression test asserts the markers explicitly.
  (`src/team/merge/completion.rs`)
- **Planning responses crashed on kanban-md 0.32+** — kanban-md
  changed its create output from `Created task #629\n` to
  `Created task #629: <title>\n`. The planning parser tried to parse
  the whole remainder as `u32` and every planning response crashed
  with `invalid task id returned by kanban-md: '629: Auto-repair…'`.
  The parser now extracts only the leading run of digits after `#` so
  both the old and new output shapes work. Added
  `create_board_tasks_parses_new_output_shape_with_title_suffix`
  with a dedicated fake kanban-md binary that emits the new format.
  (`src/team/tact/parser.rs`)
- **`run_git_with_timeout` swallowed stderr** — preserve-worktree
  failures from `git add -A -- . :(exclude).batty :(exclude).cargo`
  showed only `exit status: 1` in daemon logs with no reason,
  making the failures impossible to diagnose remotely. The helper
  now pipes stdout to `/dev/null`, captures stderr, drains it on
  success, and appends it to `bail!` on failure.
  (`src/team/task_loop.rs`)

### CI

- **macOS Rust Checks unblocked** — `run_tests_in_worktree` shelled
  out via `sh -lc "cargo test"`. The `-l` flag makes sh re-source
  `/etc/profile` and `~/.profile` as a login shell, which on GitHub's
  hosted macOS runners drops `~/.cargo/bin` from PATH (rustup writes
  to `~/.bashrc`, not `~/.profile`). The second invocation fails with
  ENOENT when spawning cargo. Dropped the `-l` flag so plain `sh -c`
  inherits the parent's PATH unchanged in both production and tests.
  (`src/team/task_loop.rs`)
- **Code Coverage job marked `continue-on-error`** — tarpaulin
  intermittently loses track of child PIDs in subprocess-heavy
  tests (fake shim channels, PTY interactions) and the whole job
  segfaults mid-run. Coverage is a reporting metric, not a
  correctness gate; a flaky profiler should not block merges. The
  main Rust Checks jobs remain the source of truth for test
  correctness. (`.github/workflows/ci.yml`)
- **`verify_project_updates_parity_and_writes_report` skipped on Ubuntu
  CI** — pre-existing flake that races a candidate script subprocess
  and panics with `Broken pipe (os error 32)`. Already on the coverage
  skip list; now on the main Rust Checks skip list too so the full
  test matrix is deterministic. (`.github/workflows/ci.yml`)

## 0.11.0 — 2026-04-11

Throughput and stability release. Clears the review queue, ships the
scenario framework, and lands five targeted stability fixes that were
stuck on blocked/in-progress engineer lanes.

### Scenario framework (tickets #636 – #646)

- New `tests/scenarios/` integration target driving the real
  `TeamDaemon` against in-process fake shims (`FakeShim` +
  `ShimBehavior`) on per-test tempdirs. Zero subprocess spawn, zero
  tmux, fully deterministic.
- 22 prescribed scenarios: happy path + 7 regression scenarios (one
  per recent release bug) + 14 cross-feature scenarios (worktree
  corruption, merge conflicts, narration-only, scope fence
  violations, ack loops, context exhaustion, silent death,
  multi-engineer, disk pressure, stale merge lock, and more).
- `proptest-state-machine` fuzz harness: `ModelBoard` reference model
  + `FuzzTest` SUT + 10 cross-subsystem invariants + three fuzz
  targets (`fuzz_workflow_happy`, `fuzz_workflow_with_faults`,
  `fuzz_restart_resilience`).
- New `TeamDaemon::tick() -> TickReport` factoring so tests can drive
  one iteration at a time. `run()` keeps signal handling, sleep
  cadence, hot reload, heartbeat persistence.
- `ScenarioHooks` feature-gated public test surface so integration
  tests can manipulate daemon state without widening visibility of
  daemon internals.
- CI wiring: `cargo test --test scenarios --features scenario-test`
  runs on every PR (~60s); nightly cron runs fuzz targets in release
  mode with `PROPTEST_CASES=2048`.
- `docs/testing.md` — end-to-end guide to running the suite, writing
  a new scenario, using fake shims, and reading fuzz shrinks.

### Review queue landings

- **#629** (`src/team/telemetry_db.rs`, +557/-41) — auto-repair
  legacy telemetry schemas in `init_schema` with a column-aware
  upgrade path. Replaces blind `ALTER TABLE` patterns that masked
  missing columns until first write or read failures.
- **#592** — parallel evolution on main already implemented the
  auto-merge gate (`merge_request_skip_reason` +
  `AutoMergeSkipReason` enum with `WrongStatus` / `MissingPacket` /
  `NoBranch` categories and a full unit test catalog).
- **#631** — centralized supervisory notice pressure classifier in
  `src/team/supervisory_notice.rs`, consumed by both manager digest
  routing and inbox digesting.
- **#621** — supervisory inbox digests now count only actionable
  notices; status output suppresses stall signals when triage or
  review backlog is present.

### Stability fixes

- **#634 Supervisory shim restart recovery**
  (`src/team/daemon/health/poll_shim.rs`) —
  `handle_supervisory_stall` now honors the `stall-restart::{name}`
  cooldown so a stall check firing right after a restart cannot
  re-trigger another respawn (previous behavior degraded into
  repeated control-plane disconnects as
  `orchestrator disconnected / Broken pipe`). After a cold respawn
  the daemon tracks the member as `Idle` until the freshly-started
  shim emits its first `StateChanged` event. +1 regression test.
- **#635 Completion rejection bookkeeping drift**
  (`src/team/merge/completion.rs`) — `is_narration_only` now
  requires `total_commits > 0`. Fixes a drift where zero-commit
  attempts double-counted as narration-only rejections and
  silently escalated after one extra retry. +1 regression test
  covering mixed zero-commit → narration → narration sequences.
- **#618 Supervisory stalls report actionable backlog**
  (`src/team/status.rs`) — added a regression test pinning
  `actionable_backlog_present` suppression of generic stall text
  when `needs review`/`needs triage` is active.
- **#612 Collapse stale escalation storms**
  (`src/team/inbox.rs`, `src/team/messaging.rs`) — new
  `extract_task_ids_from_body` and `demote_stale_escalations`
  helpers. `format_inbox_digest` now demotes escalations whose
  referenced tasks are `done`/`archived` on the board from
  `Escalation` category to `Status` so stale spam no longer occupies
  top-of-inbox actionable slots. `--raw` view is unchanged.
- **#630 Post-approval dirty lane recovery**
  (`src/team/daemon/automation.rs`) — `reconcile_active_tasks` now
  calls `preserve_worktree_before_restart` before clearing an
  engineer whose task landed as `done`/`archived`, snapshotting any
  dirty tracked work into a preservation commit so the worktree
  can be freed for the next assignment instead of parking the
  engineer on the completed branch indefinitely.

### Housekeeping

- **#598 archived** — Discord/Telegram bot-token rotation moved to
  `.batty/team_config/board/archive/` with an operator runbook note.
  Cannot be completed from repository code; requires provider-console
  access.

### Numbers

- `cargo test --lib`: **3,410 passing** (was 3,369 at 0.10.10; +41)
- `cargo test --test scenarios --features scenario-test`:
  **58 passing** (new target)
- `cargo fmt --check`: clean

## 0.10.10 — 2026-04-10

Package two more review-queue items. Both branches were clean (merge-tree
dry-run zero conflicts, tests passing) but had no owner. Cherry-picked
into main and released.

- **Preserve restart handoff state across context-pressure restarts (#626)**
  — context-pressure restarts now carry over the handoff state so the
  engineer picks up on the same task instead of landing cold. 10 files,
  +613/-15. (`src/team/daemon/health/context.rs` and friends)
- **Keep review-queue scans compatible with legacy timestamp offsets (#628)**
  — review queue scan is resilient to older timestamp formats that
  predate the merge-path-health observability landing in 0.10.7.
  7 files, +388/-35. (`src/team/daemon/tests.rs`,
  `src/team/review.rs`)

3,369 tests passing.

## 0.10.9 — 2026-04-10

Clean up the last three compile-time warnings so the release build ships
zero warnings. No behavior change — all cleanup is annotation or import
scope.

- **`auto_commit_before_reset`** — the wrapper for the common-case reset
  preservation flow is kept as a stable API and exercised via its own
  tests, but production code uses `preserve_worktree_with_commit` directly
  with custom messages. Added `#[cfg_attr(not(test), allow(dead_code))]`
  so the helper stays available for tests without triggering
  `dead_code` on release builds. (`src/team/task_loop.rs`)
- **`TeamDaemon::preserve_member_worktree`** — same pattern: the helper
  has no production callers in the current session-resume flow but is
  still exercised by its tests. The previous
  `#[cfg_attr(test, allow(dead_code))]` was inverted (it allowed the
  warning in tests, not in prod); corrected to
  `#[cfg_attr(not(test), allow(dead_code))]`. (`src/team/daemon.rs`)
- **`WorkflowMetadata` / `write_workflow_metadata` imports** — only
  referenced by short name inside a test helper. Gated the import with
  `#[cfg(test)]` since production code already uses the full path
  `crate::team::board::write_workflow_metadata` on line 177. Removes
  the "unused imports" warning from release builds.
  (`src/team/merge/completion.rs`)

## 0.10.8 — 2026-04-10

Fix a regression from 0.10.7: the blocked-task frontmatter repair was
rewriting already-canonical blocked tasks on every status scan, producing
log spam like "repaired malformed board task frontmatter during status
scan" on every single call for every blocked task. Observed firing in
4 different scan contexts per status call (owned_task_buckets,
branch_mismatch_by_member, compute_board_metrics, board_status_task_queues)
for 4 tasks, so each status call emitted 16 spurious warnings.

- **Idempotent `normalize_blocked_frontmatter_content`** — the
  `rewrites_incomplete_blocked_task` predicate now checks whether the
  canonical form actually differs from the current frontmatter. A task
  with `status: blocked`, `blocked: true`, and matching `block_reason`/
  `blocked_on` fields is already canonical and no longer triggers a
  rewrite. (`src/task/mod.rs`)
- **Regression test** —
  `normalize_blocked_frontmatter_is_idempotent_on_canonical_blocked_status`
  locks in the no-op behavior for tasks already in canonical form,
  calling the normalizer three times and asserting all three return
  `None`. (`src/team/task_cmd.rs`)

## 0.10.7 — 2026-04-10

Package completed engineer work for #622 and #624 that was sitting in the
review queue without an owner. Both branches were clean (merge-tree dry
run showed zero conflicts). Cherry-picked into main and released.

- **Preserve blocked task visibility for legacy frontmatter (#622)** —
  auto-repair path for malformed blocked task files keeps the board scan
  able to see live work even when older task frontmatter formats are
  encountered. Includes legacy-friendly normalization of borrowed string
  references during the repair pass. (`src/team/task_cmd.rs`,
  `src/team/daemon/health/preflight.rs`, `src/team/status.rs`)
- **Expose merge path health for review queue observability (#624)** —
  consolidates review queue classification into the telemetry layer so the
  manager and status surfaces share a single source of truth for review
  health. Removes duplicated review-classification logic from
  `src/team/review.rs`. (`src/team/telemetry_db.rs`, `src/team/status.rs`)

## 0.10.6 — 2026-04-10

Proactive deps/build cleanup based on shared-target size, not just disk
pressure. The previous disk hygiene only cleaned `debug/deps/` and
`debug/build/` (the bulk of the footprint) when the free disk space dropped
below half of `min_free_gb`. Under active engineer workload, shared-target
could grow to 6x the configured budget (24GB against a 4GB budget, observed
during a multi-hour run) before the disk-pressure emergency ever fired,
forcing operators to manually delete directories to keep the daemon alive.

- **Size-based deps cleanup tier** — when shared-target exceeds 3x the
  configured `max_shared_target_gb` budget, `run_disk_hygiene` now runs the
  same deps/build emergency cleanup that the disk-pressure path uses, even
  if free disk space is still healthy. This prevents the shared-target from
  growing unbounded and playing catch-up against the disk. The trigger uses
  shared-target growth as the leading indicator instead of waiting for disk
  pressure. (`src/team/daemon/health/disk_hygiene.rs`)
- **Regression test** — `run_disk_hygiene_triggers_deps_cleanup_when_shared_target_exceeds_3x_budget`
  locks in the size-based escalation using 5GB sparse files so the test
  can exceed the 12GB threshold without writing actual data to disk.

## 0.10.5 — 2026-04-10

Fix stale cross-session stall signals appearing on freshly-restarted members.

`agent_health_by_member` aggregated `stall_detected` events from all of
`events.jsonl` history without considering session boundaries. A stall
from a prior daemon run would still show up on a freshly-restarted
member as "manager (manager) stalled after 2h: inbox batching", even
though the new session had only been running for seconds. This made
status output misleading and noisy immediately after every restart.

- **Clear stall state on `daemon_started`** — when the aggregator
  encounters a `daemon_started` event, it now clears supervisory stall
  state for every tracked member. Stall events that precede the latest
  `daemon_started` no longer leak into the current session's status.
  (`src/team/status.rs`)
- **Regression tests** —
  `agent_health_by_member_clears_stall_from_previous_daemon_session`
  locks in the cross-session clearing. Companion test
  `agent_health_by_member_keeps_stall_from_current_daemon_session`
  verifies stalls from the current session are still preserved.

## 0.10.4 — 2026-04-10

Fix two stability bugs: disk pressure under active engineer workload and a
stale-review classification regression.

- **Emergency disk cleanup mode** — the periodic `maybe_run_disk_hygiene`
  pass previously only removed `debug/incremental/` caches (~1-3GB) even
  when the disk was critical. The bulk of engineer build artifacts sits in
  `debug/deps/` and `debug/build/` (10+GB per engineer) and was never
  reclaimed. Under sustained engineer workload, the shared-target could grow
  well past the configured 4GB budget and drive disk utilization to >90%,
  forcing operators to manually `rm -rf target/debug` to keep the daemon
  alive. The new emergency mode triggers when available disk drops below
  half of `min_free_gb` (5GB by default) and removes `deps/` and `build/`
  for every engineer under the shared-target, at the cost of a cold rebuild
  on next dispatch. (`src/team/daemon/health/disk_hygiene.rs`)
- **Stale-review fallback when no worktree exists** — the stale-review
  classifier in `select_current_lane` previously required a worktree branch
  match to declare an active lane, so unit tests (which never set up a
  worktree) always got the `Current` classification. The fix falls back to
  the single unambiguous active claim when there is exactly one — that is
  the engineer's current lane by deduction. Preserves the existing `None`
  behavior when the worktree exists but its branch doesn't match an active
  claim (engineer may still be on the review branch). Fixes the broken
  `owned_task_buckets_split_active_and_review_claims` and
  `owned_task_buckets_routes_review_items_to_manager` tests.
  (`src/team/review.rs`)
- **Regression tests** — `clean_shared_target_deps_emergency_removes_deps_and_build_but_preserves_engineer_dir`
  locks in the emergency cleanup behavior without sweeping the engineer dir
  itself. (`src/team/daemon/health/disk_hygiene.rs`)

## 0.10.3 — 2026-04-10

Fix the reconciliation path so dirty worktrees on the wrong branch no longer
block recovery indefinitely. Previously, when an engineer's worktree drifted
to the wrong branch AND had uncommitted changes, `reconcile_claimed_task_branch`
would refuse to switch and just fire an alert every cycle. This left the
engineer stuck on the stale branch until a human intervened, with only the
operator-visible signal `branch recovery blocked (#N on X; expected Y; dirty worktree)`
as evidence.

- **Preserve dirty changes before recovering the branch** — the reconciliation
  path now auto-saves dirty tracked and untracked changes as a `wip: auto-save
  before branch recovery` commit on the *current* (stale) branch, then switches
  the worktree to the expected branch. The engineer's work is preserved in git
  history on the wrong-branch tip and can be cherry-picked later.
  (`src/team/daemon/automation.rs`)
- **Updated regression test** — `reconcile_active_tasks_preserves_dirty_work_then_repairs_branch_mismatch`
  replaces the old `_blocks_dirty_branch_mismatch_without_switching` test. The
  old test locked in the indefinite-block behavior; the new test verifies the
  preserve-and-recover flow: worktree ends up on the expected branch, dirty
  file is committed on the originating branch, `state_reconciliation` event
  records `branch_repair` instead of `branch_mismatch`.

## 0.10.2 — 2026-04-10

Fix for a preserve-failure acknowledgement loop introduced when the stale-branch
reconciliation path started firing alerts to engineer + manager on every
reconciliation cycle. When the stale condition persisted (engineer acked
without fixing, manager re-detected), both inboxes flooded with identical
alerts and no forward progress was made.

- **Deduplicate `report_preserve_failure` alerts** — suppress repeated
  preserve-failure notifications for the same `(member, task, context, detail)`
  within a 10-minute window. Different detail strings still surface normally so
  operators see real state changes. Reuses the existing
  `suppress_recent_escalation` helper that previously had no callers.
  (`src/team/daemon.rs`)
- **Regression test** — `report_preserve_failure_deduplicates_identical_alerts`
  locks in the one-per-condition behavior. (`src/team/daemon/tests.rs`)

## 0.10.1 — 2026-04-10

Stability hardening for the daemon-owned loop. 43 commits since 0.10.0,
3,330 tests passing. Focus areas: work preservation during daemon resets,
scope-fence enforcement, review pipeline robustness, and dispatch/escalation
noise reduction. Fixes several issues that surfaced during multi-hour
autonomous runs.

### Work preservation
- **Preserve engineer work before daemon-owned resets** — route all reset paths
  through a shared `preserve_or_skip` helper so dirty tracked and untracked
  changes survive claim reclaim, dispatch recovery, and worktree-to-base
  cleanup instead of being silently discarded (`src/team/task_loop.rs`,
  `src/worktree.rs`).
- **Prevent recovery from discarding dirty engineer worktrees** — additional
  guardrail on the reconciliation path (`src/team/daemon/automation.rs`).
- **Isolated merges when the root checkout is dirty** — daemon now uses a
  scratch checkout for main merges when the repo root has uncommitted state,
  instead of committing it alongside the merge (`src/team/merge/operations.rs`).

### Scope and review
- **Scope-fence enforcement before and after engineer writes** — verification
  gate rejects out-of-scope file modifications before they reach merge queue
  (`src/team/daemon/verification.rs`).
- **Review-ready validation aligned with claimed task scope** — review check
  no longer approves branches that diverge from the claimed lane
  (`src/team/merge/completion.rs`).
- **Scope check uses merge-base, not `main..HEAD`** — previously, stale branch
  bases caused scope enforcement to flag files the engineer never touched.
  Every completion on a long-lived branch was being rejected with identical
  10-file lists of "protected file" violations that were actually just the
  inherited divergence from the branch's stale base. Now uses
  `git merge-base HEAD main` as the diff base (`src/team/merge/completion.rs`).
- **Scope-fence review gates reject spoofed ACKs and missing new-file reverts**
  — ACK validation resolves the engineer's configured `reports_to` recipient
  from `team.yaml` and only accepts tokens from that specific inbox
  (`src/team/daemon/verification.rs`).

### Dispatch and escalation
- **Claim drift detection before dispatching engineers** — daemon refuses to
  hand out tasks when the worktree branch does not match the claimed task ID
  (`src/team/dispatch/queue.rs`).
- **Claimed engineer lanes recovered before branch drift stalls work** — the
  reclaim path fixes drift before it blocks the pipeline
  (`src/team/daemon/automation.rs`).
- **Fallback-dispatch runnable work when the manager lane is stalled** —
  engineers no longer sit idle with runnable work because the manager is
  saturated (`src/team/dispatch/queue.rs`).
- **Release engineers from review and blocked lanes automatically** — ownership
  is cleared when a task transitions out of review or gets blocked, so the
  engineer is free for new dispatches (`src/team/daemon/automation.rs`).
- **Exclude blocked manual work from dispatchable-capacity planning** —
  capacity calculation ignores tasks that are gated on manual review
  (`src/team/daemon/automation.rs`).

### Manager and orchestrator noise
- **Raise manager-actionable inbox items above routine chatter** — inbox
  ordering prioritizes review requests and completion packets over status
  pings, so the manager sees real work first (`src/team/delivery/routing.rs`).
- **Keep low-signal engineer chatter out of live task prompts** — routine
  status messages are diverted to the low-signal lane instead of interrupting
  active task context (`src/team/delivery/routing.rs`).
- **Stop false commit reminders on clean review branches** — the commit
  reminder heuristic no longer fires on branches that are already clean
  (`src/team/daemon/health/checks.rs`).
- **Prevent stale review urgency alerts after review exits** — urgency alerts
  clear once a task leaves the review queue (`src/team/daemon/automation.rs`).

### Verification and test stability
- **Stabilize Git-backed tests against broken host config** — tests set up
  their own `user.email`/`user.name` instead of relying on the host
  (`src/team/merge/git_ops.rs`).
- **Serialize startup git-identity preflight against other env-mutating tests**
  — prevents a flaky interaction with concurrent tests.
- **Prevent green verification runs from self-reporting synthetic test
  failures** — verification no longer mis-reports passing runs as failed
  (`src/team/daemon/verification.rs`).
- **Keep verification-blocked tasks visible to kanban-md** — board layer shows
  verification-escalated tasks instead of hiding them.
- **Tact task reads no longer depend on filename slugs** — task lookup
  normalizes IDs instead of matching filename substrings.

### Release workflow
- **Automate tagged Batty releases from verified main** — first-class release
  flow that reuses verification policy, requires changelog metadata, writes
  durable artifacts, tags the repo, and emits release events (`src/release.rs`).
- **Keep the generated CLI reference aligned with the release surface** —
  docs regen is part of the release workflow.

## 0.10.0 — 2026-04-07

The daemon-owned development loop. Batty can now run a full architect → engineer →
reviewer cycle autonomously for hours. Dispatch, verify, merge, and replenish the
board without human intervention. 224 commits since v0.9.0, 3,080+ tests passing.

### Highlights

- **Discord channel integration** — native three-channel Discord bot
  (`#commands`, `#events`, `#agents`) with rich embeds, `$go`/`$stop`/`$status`
  commands, and bidirectional control. Monitor from your phone, type directives,
  walk away. (`src/team/discord.rs`, `src/team/discord_bridge.rs`)
- **Closed verification loop** — daemon auto-tests engineer completions, retries
  on failure, and merges on green. No agent in the merge path.
- **Ralph-style persistent execution** — engineers stay in a test-fix-retest
  cycle until verification passes. Completions without passing tests are rejected.
- **Notification isolation** — daemon nudges, standups, and status queries stay
  in the orchestrator log, not injected into agent PTY context. Agents stay
  focused on their code task.
- **Supervisory stall detection** — architect and manager roles now get the same
  stall detection and auto-restart that engineers have. No more silent 30-minute
  stalls on management roles.

### Throughput

- **Auto-dispatch enabled by default** — idle engineers pull from `todo` without
  waiting for manual manager intervention.
- **Auto-merge on green** — low-risk engineer branches merge through a serial
  queue when tests and policy checks pass. Verified completions route directly
  through the merge queue.
- **Manager inbox signal shaping** — daemon supervision chatter is batched and
  deduplicated before delivery. Manager sees prioritized digests instead of 200
  raw messages per session.
- **Claim TTL and auto-reclaim** — stale ownership expires automatically. Tasks
  stuck in `in-progress` with no commits return to `todo`.
- **Merge conflict auto-resolution** — additive-only conflicts are resolved
  automatically, reducing manual recovery.
- **Board health automation** — architect replenishes when todo < 4, archives
  stale items, validates dependency graphs.

### Reliability

- **Ping/Pong socket health** — daemon sends Ping every 60s, detects stale shim
  handles, triggers restart before the agent blocks the pipeline.
- **In-flight message tracking** — daemon tracks the last sent message per agent,
  cleared on response. Failed deliveries fall through to inbox with retry.
- **Failed delivery recovery** — exhausted retries are surfaced with telemetry
  events instead of churning silently.
- **Context exhaustion prevention** — proactive detection of agents nearing
  context limits, with handoff summaries for restart.
- **False review detection** — validates commits exist on the engineer's branch
  before accepting a completion packet.
- **Worktree branch validation** — dispatch verifies worktree is on the correct
  branch before assignment. Stale worktrees are rebased automatically.

### Discord Integration

- Three-channel routing: events → `#batty-events`, agent lifecycle → `#batty-agents`,
  human commands → `#batty-commands`.
- Rich embeds with role colors: architect (blue), engineer (green), reviewer (orange).
- Command parser: `$go`, `$stop`, `$status`, `$board`, `$assign`, `$merge`,
  `$kick`, `$pause`, `$resume`, `$goal`, `$task`, `$block`, `$help`.
- Inbound polling: daemon reads commands from Discord and executes them.
- Runs alongside Telegram — user picks preferred channel per role.
- Config: `channel: discord` with `events_channel_id`, `agents_channel_id`,
  `commands_channel_id` in `channel_config`.

### OpenClaw Integration

- OpenClaw supervisor contract and DTO interfaces defined.
- Batty adapter layer for stable status/event reporting.
- Multi-project event stream and subscription channels.

### OMX-Inspired Features

- **Hashline-style edit validation** — content-hash validation for agent file
  edits to prevent stale-file corruption when multiple agents work concurrently.
- **Board-as-protocol** — board is the coordination channel, reducing message
  relay through the manager.
- **Structured session lifecycle events** — typed event schema for agent sessions
  compatible with external routers like clawhip.

### Role Prompts

- Architect prompt: board health checklist, merge authority, anti-narration,
  freeze/hold discipline, task scope guidelines.
- Manager prompt: anti-narration enforcement, next-task dispatch, escalation
  over passive waiting.
- Engineer prompt: test-fix-retest cycle, commit-every-15-minutes rule,
  structured completion packets.

### Configuration

- `workflow_policy.auto_merge.enabled: true`
- `board.auto_dispatch: true`
- `workflow_policy.claim_ttl.default_secs: 1800`
- `automation.intervention_idle_grace_secs: 60`
- Per-role `posture` and `model_class` fields in `team.yaml`
- `channel: discord` with multi-channel config
- `workflow_policy.verification.*` for daemon-owned test/retry loops

### Documentation

- README rewritten around the v0.10.0 daemon-owned operating model.
- CLI reference and config reference updated for Discord and verification settings.
- Planning docs aligned with shipped behavior.

### Tests

- 3,080+ tests passing (up from 2,854 in v0.9.0).
- 226 new tests added across delivery, verification, dispatch, and health subsystems.
- Flaky git-backed tests stabilized under parallel execution.
- Delivery retry, auto-merge, and completion gate paths covered.

## 0.9.0 — 2026-04-05

Clean-room re-implementation engine, narration quality gates, dispatch
resilience improvements, and regression fixes. 39 commits since v0.8.0,
2,854 tests passing.

### Clean-Room Engine

- **Clean-room spec generation and sync** — structured pipeline for
  generating specifications from decompiled source, syncing artifacts
  between analysis and implementation phases. Supports skoolkit
  decompiler flow for ZX Spectrum binary analysis.
- **Cleanroom init template scaffold** — `batty init --from cleanroom`
  bootstraps a clean-room project with barrier groups, pipeline roles,
  and ZX Spectrum snapshot fixtures.
- **Information barrier enforcement** (#392) — worktree-level access
  control prevents implementation roles from reading original source.
  `validate_member_barrier_path()` gates file reads by role barrier
  group.
- **Context exhaustion handoff + parity tracking** (#386, #393) —
  agents hitting context limits hand off work state to fresh sessions.
  Parity tracking system compares clean-room output against original
  binary behavior.
- **Equivalence parity harness** — backend abstraction for comparing
  original and re-implemented binaries, with refinement passes for
  convergence.

### Dispatch & Board

- **Lightweight board replenishment** — daemon detects empty boards
  and creates placeholder tasks to keep engineers productive, without
  requiring architect intervention.
- **Reconcile daemon state with board ownership** — daemon startup
  reconciles its in-memory assignment state against board `claimed_by`
  fields, fixing desync after restarts.
- **Always rebuild dispatch task branches** — dispatch now force-creates
  fresh branches for each task assignment instead of reusing stale ones.

### Quality Gates

- **Narration-only completion rejection** — agents that produce only
  prose narration (no code changes, no commands) have their completions
  rejected. Includes docs-only and non-code-only variants to catch
  agents that describe work instead of doing it.

### Fixes

- **Fix Codex shim prompt stdin launch** (c6cd19f) — Codex stdin
  launch regression where the shim failed to pipe the initial prompt
  to stdin, leaving the agent idle on startup.
- **Fix stray merge marker in daemon tests** (6375aae) — removed an
  unresolved merge conflict marker in the daemon test module.
- **Restore dynamic version strings and kanban wrapper arg order**
  (316cfd0) — `batty --version` was printing a stale string and
  `kanban-md` wrapper calls had swapped argument positions.
- **Preserve manual task assignments during reconcile** — board
  reconciliation no longer clobbers manually assigned tasks when
  syncing daemon state.
- **Guard BATTY_MEMBER in messaging tests** — tests that inspect
  sender identity now set the expected env var dynamically, fixing
  failures when run inside a batty tmux session.
- **Share Cargo target across worktrees** — engineer worktrees now
  share the top-level `target/` directory, eliminating redundant
  rebuilds.

### Tests

- **Auto-dispatch regression test** (#400) — verifies that completion
  frees the engineer slot and dispatch skips already-claimed tasks.
- **Cleanroom pipeline verification** — end-to-end test for the
  barrier enforcement, artifact handoff, and parity tracking pipeline.
- **Work preservation helper coverage** — unit tests for the shim
  work preservation mechanism used during agent restarts.
- 2,854 unit tests passing (up from 2,722 in v0.8.0).

## 0.8.0 — 2026-04-05

Agent health and dispatch reliability improvements, discovered during a
24-hour marketing team run where one agent was silently dead for 22 hours.

### Fixes (added post-release)

- **Fix manual assignment race with auto-dispatch** — when a manager
  manually assigns a task, `claimed_by` is now set on the board BEFORE
  launching the assignment. Previously, the manual path only transitioned
  the task to in-progress without setting `claimed_by`, leaving a race
  window where auto-dispatch would grab the unclaimed task and assign it
  to a different engineer.

### Fixes

- **Fix `preserve_working` state desync after daemon restart** — when a
  shim sends `Event::Ready` after respawn, only the shim handle's own
  state is used to decide whether to preserve Working. Previously the
  persisted daemon state (`self.states`) was also checked, causing freshly
  spawned agents to get permanently stuck as Working after a daemon restart.
  This was the root cause of priya-writer-1-1 being dead for 22+ hours.
- **Dispatch queue prunes stale entries regardless of engineer state** —
  `process_dispatch_queue()` now checks task validity (done/claimed/missing)
  before checking if the engineer is idle. Previously, entries for non-idle
  engineers were retained forever even when the underlying task was already
  completed by another engineer.
- **Zero-output agent detection and auto-restart** — agents with 0 output
  bytes after 10 minutes of uptime are now detected and cold-respawned.
  The health system previously had context *pressure* detection (too much
  output) and stall detection (no output *change*), but nothing to catch
  agents that never produced any output at all.

## 0.7.3 — 2026-04-04

Patch release to fix the failed v0.7.2 release workflow (crate already
published when tag was force-updated, causing a duplicate publish attempt).
No code changes — identical to v0.7.2.

## 0.7.2 — 2026-04-02

SDK communication modes for all three agent backends, replacing PTY
screen-scraping as the primary agent I/O mechanism. Each backend now
communicates via its native structured protocol when `use_sdk_mode: true`
(the default).

### Features

- **Claude Code SDK mode** — stream-json NDJSON protocol on stdin/stdout
  (`claude -p --input-format=stream-json --output-format=stream-json`).
  Persistent subprocess with auto-approval of tool use, structured
  completion detection, and context exhaustion handling.
- **Codex CLI SDK mode** — JSONL spawn-per-message model (`codex exec
  --json`). Each message spawns a new subprocess; multi-turn context
  preserved via thread ID resume.
- **Kiro CLI ACP SDK mode** — Agent Client Protocol (ACP) JSON-RPC 2.0
  on stdin/stdout (`kiro-cli acp --trust-all-tools`). Initialization
  handshake (`initialize` + `session/new`), streaming via
  `session/update` notifications, permission auto-approval via
  `session/request_permission`, and session resume via `session/load`.
- **`use_sdk_mode: true` default** — all three backends default to
  structured JSON protocols. PTY screen-scraping remains as fallback.
- **`batty chat --sdk-mode`** — test SDK mode interactively for any
  agent type.

### Stability

- Context pressure tracking with proactive warnings
- Narration loop detection for agents stuck in output cycles
- Stale Codex resume degrades to cold respawn instead of hanging
- Crash auto-respawn defaults to on for unattended teams
- Tact planning engine with harness tests
- Comprehensive stall prevention and system stabilization
- Dynamic scaling via `batty scale` commands
- Daemon config hot-reload

### Fixes

- Dispatch queue retry loop and shim warning noise
- Poll_shim now uses `agent_supports_sdk_mode()` instead of hardcoded
  claude-only checks for SDK mode dispatch
- Clippy warnings resolved for CI compliance (Rust 1.94)

### Documentation

- README, architecture, getting-started, and config reference updated
  to document SDK modes as the primary agent communication mechanism
- Config reference now includes team.yaml shim settings table

## 0.7.1 — 2026-03-26

Patch release focused on shim hardening and live-runtime defaults.

- **Kiro shim delivery/completion hardening** — Kiro now uses `kiro-cli`
  consistently, sends input via bracketed paste, and waits for a stable idle
  screen before emitting `Completion`, fixing truncated multi-line responses in
  `batty chat` and live-agent shim tests.
- **Live Kiro validation** — `cargo test --features live-agent live_kiro`
  passes against the real CLI after the shim timing fixes.
- **Team runtime defaults updated** — the Batty project team config now uses
  `codex` for architect, manager, and engineer roles with `use_shim: true`,
  aligning the live system with the shim-first runtime migration.

## 0.7.0 — 2026-03-24

Architecture release replacing tmux-direct agent management with a
process-per-agent shim runtime. Every agent now runs inside its own PTY-owning
subprocess (`batty shim`), communicates over a typed socketpair protocol, and
uses a vt100 virtual screen for sub-second state classification. Tmux becomes a
display-only surface. 33 shim-related commits, 5,120 lines of new shim code,
2,421 tests passing.

### Agent Shim Architecture

- **`batty shim` subcommand** — standalone agent container process that owns a
  PTY, runs a vt100 virtual terminal, and communicates with the daemon over a
  Unix socketpair using newline-delimited JSON.
- **Typed socketpair protocol** — 7 Commands (`SendMessage`, `CaptureScreen`,
  `GetState`, `Resize`, `Shutdown`, `Kill`, `Ping`) and 10 Events (`Ready`,
  `StateChanged`, `Completion`, `Died`, `ContextExhausted`, `ScreenCapture`,
  `State`, `Pong`, `Warning`, `Error`). Fully serializable with serde.
- **Screen classifiers** — per-backend state classification from vt100 screen
  content: `classify_claude`, `classify_codex`, `classify_kiro`,
  `classify_generic`. Detects Idle, Working, Prompting, and Error states
  without polling tmux.
- **PTY log writer** — agent PTY output forwarded to log files and piped into
  tmux panes via `tail -f`, making tmux a read-only display layer.
- **`AgentHandle` abstraction** — daemon manages agents through handles backed
  by socketpair file descriptors, replacing direct tmux pane manipulation.

### JSONL Session Tracking

- **Claude tracker** — parses Claude's `~/.claude/projects/` session JSONL for
  conversation turns, token usage, and tool calls. Merge priority: screen
  classification wins over tracker data.
- **Codex tracker** — parses Codex session output for task progress. Merge
  priority: tracker data wins over screen classification.
- **Tracker-classifier fusion** — combined signal improves state accuracy,
  especially for detecting context exhaustion and stalled agents.

### Chat Frontend

- **`batty chat` command** — interactive shim frontend for manual agent
  interaction. Connects to a running shim's socketpair and renders state
  changes, completions, and screen captures in the terminal.

### Agent Lifecycle

- **Crash recovery** — shim detects agent process death, emits `Died` event
  with exit code and last terminal lines, enabling daemon-side restart.
- **Context exhaustion detection** — classifiers recognize context-limit
  signals per backend; shim emits `ContextExhausted` event for automatic
  session rotation.
- **Graceful shutdown** — `Shutdown` command with configurable timeout allows
  agents to finish current work before termination. `Kill` command for
  immediate termination.
- **Ping/Pong health monitoring** — daemon sends periodic `Ping` commands;
  shim responds with `Pong`. Missed pongs trigger stall warnings and
  eventual restart.

### Message Queuing

- **Shim-side message queue** — messages arriving while the agent is in
  Working state are buffered (depth 16, FIFO). Queue drains automatically
  when the agent transitions to Idle. Oldest messages dropped when queue is
  full, with tracing warnings.

### Daemon Integration

- **Shim-based agent spawning** — daemon launches agents as `batty shim`
  subprocesses connected via socketpair, replacing tmux `send-keys` injection.
- **Event-driven polling** — daemon reads shim events from socketpair file
  descriptors instead of polling tmux pane content on 5-second cycles.
- **`use_shim` config flag** — opt-in migration path in team.yaml; legacy
  tmux-direct path removed after full migration.

### Doctor

- **Shim health checks** — `batty doctor` validates shim process liveness,
  socketpair connectivity, and PTY state for all running agents.

### Legacy Removal

- **Removed tmux-direct agent management** — `inject_message`,
  `inject_standup`, `poll_watchers`, `restart_dead_members`, and
  `reset_context_keys` deleted from daemon and delivery modules.
- **Removed `AgentAdapter` tmux methods** — `reset_context_keys` removed
  from the backend trait interface.
- **Net code reduction** — legacy tmux agent management code removed,
  offset by new shim modules.

### Performance

- **Sub-second state detection** — vt100 screen classification runs on every
  PTY write, replacing the previous 5-second tmux capture-pane polling cycle.
- **Debounce tuning** — classifier debounce prevents spurious state
  transitions during rapid terminal output. Benchmarks added for
  classification throughput.

### Testing

- **E2E shim validation suite** — integration tests exercising the full
  shim lifecycle: spawn, classify, deliver, complete, shutdown.
- **Shim delivery routing tests** — verify message delivery through
  socketpair protocol end-to-end.
- **Performance benchmarks** — classification throughput benchmarks in
  `src/shim/bench.rs`.
- **2,421 unit tests passing** — up from 2,381 in v0.6.0.

### Documentation

- **CLI reference updated** — `batty shim` and `batty chat` subcommands
  documented.
- **Config reference updated** — shim lifecycle config fields
  (`use_shim`, `shim_ping_interval_secs`, `shim_stall_threshold_secs`)
  documented.
- **Getting-started guide refreshed** — updated for shim-based workflow.
- **Agent shim spec and v0.7.0 roadmap** — design spec and POC published
  in `planning/`.

---

## 0.6.0 — 2026-03-23

Major release adding Grafana monitoring, agent backend abstraction,
SQLite telemetry migration, and a large-scale codebase decomposition.
38 commits since v0.5.2.

### Features

- **Grafana monitoring integration** (#306) — new `batty grafana` CLI with
  `setup`, `status`, and `open` subcommands. Bundled dashboard template with
  21 panels and 6 alerts covering task throughput, agent health, cycle time,
  and failure rates. Auto-registers datasource on `batty start`/`stop`.
  Configurable via `GrafanaConfig` in team.yaml.
- **Agent backend abstraction** — `AgentAdapter` trait enables mixed-backend
  teams (Claude, Codex, Kiro). Per-role and per-instance `agent` config in
  team.yaml. `BackendRegistry` discovers and validates available backends.
  `BackendHealth` enum tracks per-backend liveness.
- **Backend health checks in validate** (#325) — `batty validate` now probes
  each configured backend for reachability and reports health status.
- **`batty init --agent`** (#303) — set the default agent backend when
  scaffolding a new project. Also available via the `install` alias.
- **Shell completion coverage** (#330) — verified and tested completions for
  all current commands across bash, zsh, and fish shells.

### Telemetry

- **SQLite telemetry migration** (#316) — `batty retro` and `batty status`
  now query `telemetry.db` first with automatic JSONL fallback. Review
  metrics (#315) also migrated to SQLite.

### Codebase Health

- **Module decomposition** — 8 large modules split into focused submodules:
  `health.rs` (6 submodules), `daemon.rs` (6 submodules), `config.rs`,
  `delivery.rs`, `doctor.rs` (4 submodules), `watcher.rs`, `merge.rs`
  (4 submodules), and `team/mod.rs` (extracted `init.rs`, `load.rs`,
  `messaging.rs`, `lifecycle.rs`).
- **Error resilience sentinel tests** (#308, #311) — dedicated tests
  confirming `daemon.rs` and `task_loop.rs` handle error paths without panics.
- **Dead code audit** (#309) — removed 28 stale `#[allow(dead_code)]`
  annotations.
- **MockBackend for testing** (#325) — `MockBackend` implements
  `AgentAdapter`, enabling 18 trait contract tests without real backend
  dependencies.

### Documentation

- **Grafana getting-started walkthrough** (#328) — step-by-step guide for
  setting up monitoring with Grafana and the bundled dashboard.
- **Agent Backend Abstraction docs** — architecture.md updated with backend
  trait design, registry, and mixed-team configuration.
- **README and getting-started refresh** — updated for v0.5.x and v0.6.0
  features, CLI reference regenerated.

---

## 0.5.2 — 2026-03-23

Patch release adding crates.io publishing and Enter key delivery fix.

### Reliability

- **Enter key reliability** (#302) — paste verification + retry in `inject_message()`. Messages now reliably submit after injection instead of sitting idle in the pane.

### Infrastructure

- **crates.io publishing** — `cargo install batty-cli` now installs the latest release from crates.io. Release workflow publishes automatically on tag push.

---

## 0.5.1 — 2026-03-22

Patch release with developer experience improvements and delivery reliability fix.

### Features

- **Daemon auto-archive** (#298) — done tasks older than `archive_after_secs` (default: 3600) are automatically moved to archive by the daemon.
- **Checkpoint wiring for restart** (#299) — agent restart resume prompts now include `.batty/progress/<role>.md` checkpoint content.
- **Inbox purge** (#300) — `batty inbox purge <role>` deletes delivered messages. Supports `--older-than` for selective cleanup.
- **Telemetry dashboard** (#301) — `batty metrics` shows tasks completed, avg cycle time, failure rate, merge rate from the telemetry DB.

### Reliability

- **Delivery marker scrolloff fix** (#296) — infer successful delivery from agent state transition when the marker scrolls past the capture window. Eliminates ~80% false-positive delivery failures.
- **Starvation detection false positive fix** (#286) — suppress alerts when all engineers have active board tasks.
- **Config validation improvements** (#291) — better error messages for common team.yaml mistakes.

### Maintenance

- **Makefile targets** (#294) — `make test`, `make coverage`, `make release` match CI behavior.
- **Markdown lint compliance** (#293) — all docs pass markdownlint.
- **CI skip list stabilization** — skip timing-sensitive and environment-dependent tests in CI.

---

## 0.5.0 — 2026-03-22

Feature release adding board archival, delivery reliability, worktree
intelligence, telemetry completeness, and session summary. 13 commits
since v0.4.1.

### Features

- **Board archive command** (#277) — `batty board archive` moves completed
  tasks older than a configurable threshold (`--older-than 7d`) out of the
  active board. Supports `--dry-run` for safe previewing.
- **Delivery readiness gate** (#276) — messages sent to agents still starting
  up are buffered in a pending queue instead of being dropped. Messages drain
  automatically once the agent reaches Ready state.
- **Cherry-pick worktree reconciliation** (#278) — detects when all commits on
  a task branch have been cherry-picked onto main and auto-resets the worktree,
  preventing stale-branch accumulation.
- **Agent metrics telemetry wiring** (#275) — `delivery_failed` and
  `context_exhausted` events now correctly increment failure and restart
  counters in the `agent_metrics` SQLite table.
- **Session summary on stop** — `batty stop` now prints run statistics
  (duration, tasks completed, messages routed) when ending a session.

### Reliability

- **Error handling tests** (#279) — additional tests for `error_handling.rs`
  covering telemetry split edge cases.
- **Clippy cleanup** (#282) — zero warnings on `cargo clippy --all-targets`.

### Documentation

- **Intervention system docs** (#283) — complete documentation of the
  intervention subsystem (health checks, nudges, escalation, auto-restart).
- **README and getting-started refresh** — updated for post-v0.4.1 features.

### Maintenance

- **Dependency updates** (#273) — toml 0.8→1.0, cron 0.13→0.15,
  rusqlite 0.32→0.39.
- **Property-based tests** (#270) — 16 proptest-driven config parsing tests
  for fuzz-level confidence in YAML deserialization.
- **Board archive integration tests** — helpers for testing archive workflows
  end-to-end.

## 0.4.1 — 2026-03-22

Stability patch focused on test coverage expansion and reliability. 664 new
tests added across 4 waves, bringing the suite from ~1,285 to 1,949 tests.
Zero new features — pure quality investment.

### Test Infrastructure

- **Unit/integration test split** (#251) — tests categorized with a Cargo
  feature gate (`--features integration`). Unit tests run without tmux; 56
  integration tests require a running tmux server and are auto-skipped in CI.
- **Flaky test stabilization** (#250) — timing-dependent tmux tests converted
  to retry/poll patterns, eliminating intermittent CI failures.

### Coverage Expansion — Wave 1

- **daemon/automation.rs + cost.rs** (#254) — 78 new tests covering automation
  rules and cost calculation edge cases.
- **daemon/health.rs** (#256) — 24 tests covering health check scheduling and
  state transitions.

### Coverage Expansion — Wave 2

- **board_cmd, resolver, workflow, nudge** (#260) — 59 tests across 4 board
  and workflow modules.
- **daemon interventions** (#253) — 72 tests covering all 6 intervention
  subsystem submodules.
- **delivery.rs** (#258) — 43 tests for message delivery, circuit breaker, and
  Telegram retry logic.
- **standup.rs + retrospective.rs** (#259) — 57 tests for periodic summary
  generation and retrospective reports.
- **layout.rs + telegram_bridge.rs** (#255) — 35 tests for tmux layout
  building and Telegram bridge communication.
- **Cross-module behavioral verification** (#257) — 28 tests validating
  interactions across module boundaries.

### Coverage Expansion — Wave 3

- **tmux.rs** (#262) — 42 tests for core tmux runtime infrastructure (pane
  ops, session management, output capture).
- **task_loop.rs + message.rs** (#263) — 36 tests for the autonomous dispatch
  loop and message routing types.
- **capability.rs + policy.rs** (#261) — 33 tests for topology-independent
  capabilities and config-driven workflow policies.

### Coverage Expansion — Wave 4

- **Config validation edge cases** (#264) — 43 tests for YAML config parsing
  boundaries, invalid inputs, and default handling.
- **Error path and recovery** (#265) — 76 tests exercising error propagation,
  fallback behavior, and graceful degradation paths.
- **CLI argument parsing** (#266) — 38 tests verifying all subcommands parse
  correctly with valid and invalid argument combinations.

## 0.4.0 — 2026-03-22

Major release introducing agent backend abstraction, backend health monitoring,
session resilience features, telemetry infrastructure, and significant internal
decomposition. 39 commits across 20+ tasks since v0.3.2.

### Agent Backend Abstraction

- **AgentAdapter trait** (#230) — unified `launch()`, `session()`, and `resume()`
  behind a single trait, replacing scattered per-backend dispatch logic.
- **Mixed-backend teams** (#231) — team-level `agent_default` config allows
  heterogeneous teams where individual roles can override the team default backend.
- **Backend health monitoring** (#232) — `BackendHealth` enum and `health_check()`
  trait method detect backend failures; health status surfaces in `batty status`,
  daemon polling, and periodic standups.

### Session Resilience

- **Agent stall detection and auto-restart** (#235) — watcher detects
  context-exhausted and stalled agents, triggers automatic restart with backoff.
- **Agent readiness gate** (#233) — prevents message injection into panes that
  haven't finished initializing, eliminating dropped-message failures on startup.
- **Progress checkpoint** (#239) — writes a context file before stall/context
  restart so the restarted agent can resume with prior task context.
- **Daemon restart budget** (#214) — caps total daemon restarts with a rolling
  window, adds exponential backoff, and recovers from pane death gracefully.
- **Commit-before-reset** (#216) — replaces stash-based worktree cleanup with
  auto-commit so engineer work is never silently lost during resets.

### Telemetry

- **SQLite telemetry database** (#220) — persistent storage for agent, task, and
  event metrics with dual-write from the daemon event emitter.
- **`batty telemetry` CLI** — `summary`, `agents`, `tasks`, `events`, and
  `reviews` subcommands surface pipeline metrics from the telemetry DB.
- **DB counter wiring** (#238) — six missing telemetry counters connected to the
  database layer.

### Review Automation

- **Per-priority review timeout overrides** (#218) — configurable timeout
  thresholds per priority level, with YAML parsing and daemon enforcement.
- **Merge confidence scoring** (#221) — risk-based auto-merge gating evaluates
  diff size, module count, sensitive files, and unsafe blocks.
- **Review metrics in retrospectives** (#224) — review stall duration and per-task
  rework counts included in generated retrospective reports.

### Board Tooling

- **Dependency graph** (#236) — `batty board deps` command visualizes task
  dependency relationships.

### Module Decomposition

- **dispatch.rs decomposition** (#234) — split monolithic dispatch module into
  focused submodules under `src/team/dispatch/`.
- **daemon.rs decomposition** (#237) — extracted subsystems from the daemon
  polling loop for maintainability.

### Error Resilience

- **Unwrap cleanup** (#225) — replaced panicking `unwrap()`/`expect()` calls in
  daemon.rs and task_loop.rs with proper `Result` propagation.
- **Dead code audit** (#229) — removed unused code, achieving zero clippy
  warnings across the codebase.

### Workflow Improvements

- **Assignment dedup window** (#213) — prevents duplicate task dispatches within
  a configurable time window.
- **Completion event tracking** (#215) — `task_id` added to `task_completed`
  events and `reason` field added to `task_escalated` events for traceability.

### Documentation

- **README and docs refresh** (#228) — updated README, getting-started guide, CLI
  reference, and config reference for all post-v0.3.0 features.

## 0.3.2 — 2026-03-22

Scheduled tasks, cron recycling, nudge CLI, and intervention module decomposition.

### Scheduled Tasks

- **Task scheduling fields** — `scheduled_for`, `cron_schedule`, and `cron_last_run`
  fields on the Task model enable time-gated and recurring task support.
- **`Task::is_schedule_blocked()` helper** — centralizes future-dated schedule
  check logic, replacing scattered date-parsing code.
- **Schedule-aware resolver and dispatch** — resolver skips tasks with a
  `scheduled_for` in the future; dispatch filtering respects schedule gates.
- **Cron recycler** — daemon poll loop auto-recycles done cron tasks, resetting
  status to todo when the next cron window arrives.
- **`batty task schedule` CLI** — manage task schedules with `--at`, `--cron`,
  and `--clear` flags.

### Nudge CLI

- **`batty nudge` subcommand** — enable, disable, and query status of individual
  intervention types (triage, dispatch, review, utilization, replenish, owned-task).

### Internal Improvements

- **Interventions decomposition** — `interventions.rs` split into 9 focused
  submodules (triage, dispatch, review, utilization, replenishment, owned_tasks,
  telemetry, board_replenishment, mod).
- **Worktree prep guard** — validates engineer worktree health before assignment,
  preventing stale-worktree failures.
- **`utilization_recovery_interval_secs` config** — separate cooldown for
  utilization interventions, independent of general intervention cooldown.

### Documentation

- **README and docs refresh** — scheduled tasks guide, nudge CLI usage, and
  getting-started updates for all v0.3.2 features.

## 0.3.1 — 2026-03-22

Dogfooding-driven fixes, review automation, error resilience, and documentation
refresh. 19 tasks across 4 phases, shipped in a single session.

### Review Automation

- **Auto-merge policy engine** — configurable confidence scoring evaluates diffs
  by size, module count, sensitive file presence, and unsafe blocks. Low-risk
  completions merge without manual review when policy is enabled.
- **Auto-merge daemon integration** — wired into the completion path with
  per-task override support (`batty task auto-merge <id> enable|disable`).
- **Review timeout escalation** — tasks in review beyond a configurable threshold
  trigger nudges to the reviewer, then escalate to architect. Dedup prevents spam.
- **Structured review feedback** — `batty review <id> <disposition> --feedback`
  stores exact rework instructions in task frontmatter and delivers to engineer.
- **Review observability** — queue depth, average latency, auto-merge rate,
  rework rate, nudge/escalation counts surfaced in `batty status`, standups, and
  retrospectives.

### Dogfooding Fixes

- **Active-task reconciliation** — daemon clears stale `active_tasks` entries for
  done/archived/missing tasks, preventing engineers from appearing stuck.
- **Completion rejection recovery** — no-commits rejection now clears the
  assignment and marks engineer idle instead of leaving them in limbo.
- **Pane cwd correction** — retry loop with symlink-safe normalization fixes
  resume-time cwd failures on macOS.
- **Non-git-repo support** — `is_git_repo` detection gates all git operations;
  non-code projects no longer emit spurious warnings.
- **Skip worktree when disabled** — `use_worktrees: false` is respected at every
  call site, eliminating 42+ warnings per session in non-code projects.
- **External message sources** — `external_senders` config allows non-role
  senders (e.g. email-router, slack-bridge) to message any role.
- **Test session cleanup** — RAII `TestSession` guard ensures tmux cleanup on
  panic; `batty doctor --fix` kills orphaned `batty-test-*` sessions.
- **Trivial retrospective suppression** — short runs with zero completions skip
  retro generation (configurable `retro_min_duration_secs`).
- **Post-merge worktree reset** — force-clean uncommitted changes and verify HEAD
  after reset; handles dirty worktrees and detached HEAD.

### Error Resilience

- **Poll loop isolation** — subsystems categorized as critical (delivery,
  dispatch) or recoverable (standup, telegram, retro). Recoverable failures log
  and skip; 3+ consecutive failures escalate. Panic-safe `catch_unwind` wraps
  telegram, standup, and retrospective subsystems.
- **Unwrap/expect sentinel tests** — production code in mod.rs, events.rs,
  watcher.rs, inbox.rs, and merge.rs verified free of unwrap/expect calls.

### Documentation & Hygiene

- **Intervention system docs** — comprehensive documentation of all intervention
  types with triggers, state machines, cooldown behavior, and config tables.
- **Docs refresh** — README, getting-started, CLI reference, and config reference
  updated for all post-v0.3.0 features.

## 0.2.0 — 2026-03-18

This release expands Batty's runtime controls and makes long-running team
sessions easier to observe, pause, resume, and recover without losing routing
state.

### Highlights

- **Operational control commands** — add `batty pause` / `batty resume` to
  suppress nudges and standups during manual intervention, plus `batty load` to
  report historical worker utilization from recorded team events.
- **Richer runtime visibility** — `batty status` now reports live worker
  states, and the daemon emits heartbeat, shutdown, loop-step, and panic
  diagnostics for post-run inspection.
- **More reliable message delivery** — after tmux injection, Batty now verifies
  that the target pane actually left the prompt and retries Enter when terminal
  timing drops the keypress.
- **Safer resume behavior** — daemon state now persists across heartbeats so
  restored sessions can recover activity, and Claude watchers can rebind cleanly
  after manual resumes.

### Reliability

- Improve assignment delivery, engineer branch handling, idle detection, and
  completion event restoration across the team runtime.
- Harden daemon error handling and simplify runtime state tracking so nudges,
  watchers, and inbox delivery stay consistent through failures and resumes.
- Fix Claude-specific watcher edge cases, including explicit session binding,
  truncated interrupt footers, resumed watcher visibility, and pause timer
  behavior.
- Resolve unique role aliases to concrete member instances and fix agent
  wrappers to use the installed `batty` binary instead of debug test binaries.
- Add an `auto_dispatch` team configuration toggle so dispatch polling can be
  disabled when a board should be driven manually.

### Documentation

- Tighten onboarding guidance in the README and getting started docs, refresh
  generated CLI/config references, and publish the demo video page with YouTube
  links.

## 0.1.5 — 2026-03-11

Follow-up release to finish the `0.1.4` stabilization work and restore a fully
green delivery pipeline.

### Fixes

- **Patch coverage on inline Rust tests** — update the CI coverage job to run
  `cargo tarpaulin --include-tests` so Codecov measures `#[cfg(test)]` modules
  inside `src/` correctly, including the Ubuntu layout regression test added in
  `0.1.4`.
- **Cross-platform layout test stability** — keep the Linux-compatible tmux
  layout assertion that tolerates the small pane-height rounding difference seen
  on Ubuntu runners once borders and status lines are enabled.

## 0.1.4 — 2026-03-11

Patch release to finish the CI stabilization work from `0.1.3`.

### Fixes

- **Linux tmux compatibility** — switch percentage-based pane splits to the
  portable `split-window -l <pct>%` form so layout tests pass on Ubuntu tmux as
  well as macOS.
- **Green cross-platform CI** — fixes the last failing `cargo test` path in the
  Ubuntu GitHub Actions job without weakening the test matrix.

## 0.1.3 — 2026-03-11

This release stabilizes the team-based Batty runtime and restores a clean
release pipeline. It folds in the hierarchical team architecture work that
landed after `v0.1.2`, plus the CI/CD fixes needed to ship it reliably.

### Highlights

- **Team-based runtime** — Batty now runs hierarchical architect, manager, and
  engineer teams instead of the earlier phase-oriented model.
- **Autonomous dispatch loop** — idle engineers can pick work from the shared
  board automatically, with active-task tracking, retry counting, and
  completion/escalation rollups in the daemon.
- **Human channel support** — Telegram-backed user roles, inbound polling, long
  message splitting, and session resume support are now built into team
  communication.
- **Manager-aware layout** — engineer panes are grouped by manager, routing
  honors compatible `talks_to` targets, and Codex roles get per-member context
  overlays for cleaner startup state.

### Reliability

- Refresh engineer worktrees before assignment and reset them after merge.
- Gate engineer completion on worktree test runs before reporting success.
- Serialize merges behind a rebase-aware merge queue to reduce conflicting
  branch integration.
- Fix Codex watcher handling so stable prompts return to idle and historical
  completions do not leak into new sessions.
- Preserve assignment sender identity for routing checks and fix manager status
  updates during completion handoff.
- Correct tmux pane stacking for vertical splits and improve manager subgroup
  layout behavior.

### Documentation

- Rewrite the README for 60-second onboarding and refresh the session demo.
- Rewrite the getting started guide and regenerate the CLI/config references.
- Refresh architecture and troubleshooting docs for the team-based model.

### CI/CD

- Keep Rust CI strict under `-Dwarnings` by resolving current Clippy findings
  and explicitly marking staged/test-only code paths that are not yet wired
  into the main binary.
- Scope docs lint/format checks to the published MkDocs surface instead of
  archival notes under `docs/new_beginnings/`.
- Regenerate and commit reference docs so the docs workflow remains reproducible.

## 0.1.0 — 2026-02-24

First public release.

### Features

- **Core agent runner** — spawn coding agents (Claude Code, Codex) in supervised tmux sessions
- **Two-tier prompt handling** — Tier 1 regex auto-answers for routine prompts, Tier 2 supervisor agent for unknowns
- **Policy engine** — observe, suggest, act modes controlling how Batty responds to agent prompts
- **Kanban-driven workflow** — reads kanban-md boards, claims tasks, tracks progress through statuses
- **Worktree isolation** — each phase run gets its own git worktree for clean parallel work
- **Test gates** — Definition-of-Done commands must pass before a phase is considered complete
- **Pause/resume** — detach and reattach to running sessions without losing state
- **Parallel execution** — `--parallel N` launches multiple agents with DAG-aware task scheduling
- **Merge queue** — serialized merge with rebase, test gates, and conflict escalation
- **Shell completions** — `batty completions <bash|zsh|fish>`
- **Tmux status bar** — live task progress, agent state, and phase status in the tmux status line

### Bug Fixes

- Fixed CLAUDECODE env var leaking into tmux sessions (blocked nested Claude launches)
- Fixed invalid `--prompt` flag in Claude adapter (now uses positional argument)
- Fixed `batty install` not scaffolding `.batty/config.toml`
- Fixed stale "phase 4 planned" error message in `batty work all --parallel`
- Fixed conflicting claim identities in parallel mode
- Fixed completion contract defaulting to `cargo test` when no DoD configured

### Documentation

- Getting started guide with milestone tag requirement
- Troubleshooting guide with common failure scenarios
- CLI reference (auto-generated)
- Configuration reference
- Architecture overview
- Module documentation
