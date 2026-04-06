# OpenClaw Supervision Rollout Roadmap

## Goal

Introduce OpenClaw supervision into Batty without destabilizing actively-used teams.

The rollout should:

- ship in small, reversible steps
- reuse Batty's existing adapter, shim, escalation, and telemetry architecture
- keep the operator-facing surface small and understandable
- make it obvious what is MVP versus what is deferred

This plan assumes OpenClaw enters Batty as a supervised backend integration, not a parallel runtime. Batty remains the system of record for team topology, board state, messaging, and operator controls.

## Non-Goals

Not in scope for the first rollout:

- replacing Batty's team daemon with OpenClaw
- introducing a second board or second routing system
- making every existing escalation path OpenClaw-aware on day one
- fully autonomous remediation before the supervision contract is stable
- broad UI changes beyond minimal status visibility and docs

## Guiding Constraints

### Incremental shipping

Each phase must be independently shippable behind an explicit config switch. Teams should be able to stop at any phase and still have a coherent operator experience.

### Narrow release surface

Operators should only need to understand three concepts:

1. OpenClaw can be selected as an agent backend.
2. Batty can ask OpenClaw for supervision decisions.
3. Escalation can remain advisory before it becomes automatic.

### Batty stays authoritative

Batty continues to own:

- team configuration
- worktree lifecycle
- task routing and board state
- escalation destinations
- telemetry and release gating

OpenClaw should initially provide decisions and supervision output, not primary control over runtime state.

## Phase 1: Contract + Adapter

### Objective

Define the interface Batty needs from OpenClaw and make OpenClaw selectable through the existing agent adapter path.

### Deliverables

- `OpenClawAdapter` implementing `AgentAdapter`
- explicit launch contract for prompt, cwd, env, resume behavior, and health check
- prompt/status detection contract for supervision-relevant states
- config validation for `agent: openclaw`
- unit tests for spawn config, prompt patterns, input formatting, and health detection
- operator docs covering how OpenClaw is launched and what Batty expects from it

### MVP scope

- support OpenClaw as a backend in a single engineer or single-role test team
- treat OpenClaw like any other backend from Batty's point of view
- support health check, launch, stdin injection, and basic prompt classification
- allow manual operator fallback if the adapter cannot classify a state cleanly

### Deferred from Phase 1

- automatic supervision decisions
- escalation policy integration
- advanced resume semantics beyond what OpenClaw can prove reliably
- mixed-backend production defaults
- auto-restart behavior tuned specifically for OpenClaw

### Exit criteria

- `batty validate` accepts OpenClaw-backed team configs
- `batty start` can launch an OpenClaw-backed member in shim mode
- adapter tests are stable and deterministic
- docs are clear enough that an operator can enable or disable the backend without reading source

## Phase 2: Supervisor MVP

### Objective

Wire OpenClaw into Batty's existing supervision flow as a bounded decision engine for unresolved prompts and supervision events.

### Deliverables

- supervision contract document for Batty -> OpenClaw request and OpenClaw -> Batty response
- narrow set of supervisor call sites:
  - unresolved prompt handling
  - ambiguous permission/request classification
  - "needs human / retry / continue / deny" style decision output
- parser and validation layer that rejects malformed or non-machine-readable responses
- tracing and telemetry for every OpenClaw supervision call
- feature flag or config gate to keep OpenClaw supervision disabled by default

### MVP scope

- advisory or low-authority supervision only
- OpenClaw can recommend actions; Batty still enforces policy
- failure path is simple: reject output, log event, fall back to existing Batty handling or human escalation
- supervisor decisions are limited to a small enum-like response surface

### Deferred from Phase 2

- task-aware long-horizon planning
- automatic multi-step remediation
- broad integration across every daemon intervention
- silent self-healing on supervisor output alone

### Exit criteria

- supervision calls are observable in logs and telemetry
- malformed supervisor responses fail closed
- no regression to non-OpenClaw teams when the feature is disabled
- a pilot team can run with OpenClaw supervision enabled and maintain normal dispatch/merge behavior

## Phase 3: Escalation Policy and Automation

### Objective

Promote OpenClaw from advisory supervision to a controlled participant in Batty's escalation and recovery policy.

### Deliverables

- policy mapping from Batty events to OpenClaw supervision actions
- configurable authority tiers, for example:
  - `off`
  - `advise`
  - `suggest`
  - `act_limited`
- scoped automation for a small set of recoveries:
  - retry once
  - request checkpoint/handoff
  - recommend restart
  - recommend manager escalation with structured reason
- cooldowns, dedupe keys, and replay protection for automated actions
- escalation message templates that explain when OpenClaw acted versus when Batty only observed

### MVP scope

- automation only for low-risk, reversible actions
- manager/operator visibility on every automated action
- per-team opt-in, not global default

### Deferred from Phase 3

- autonomous merge/review decisions driven by OpenClaw
- agent-to-agent negotiation loops
- automatic edits to policy or topology
- opaque "AI decided" actions without structured reason codes

### Exit criteria

- automation reduces repeated manual interventions on pilot teams
- escalation volume does not spike due to noisy or duplicate OpenClaw actions
- operators can explain why an action happened from logs/telemetry alone

## Phase 4: Hardening, Docs, Release

### Objective

Turn the integration from pilot-only capability into a supported release option.

### Deliverables

- failure-mode test matrix for adapter, supervisor parsing, timeout handling, and fallback behavior
- integration coverage for mixed-backend teams
- docs updates:
  - architecture
  - module reference
  - configuration reference
  - troubleshooting
  - migration notes
- release checklist and operator runbook
- telemetry/dashboard additions for OpenClaw supervision outcomes
- default recommendations for when teams should and should not enable the feature

### MVP scope

- documented beta/experimental release
- explicit "safe default" configuration examples
- release notes with rollback instructions

### Deferred from Phase 4

- making OpenClaw supervision the default for all teams
- compatibility guarantees for every external OpenClaw version without validation
- broad policy auto-tuning

### Exit criteria

- support burden is acceptable for active teams
- rollout checklist has been exercised on at least one real team
- telemetry shows measurable value instead of just more system complexity

## Release Shape

Keep the release surface understandable by shipping three operator-visible toggles at most:

- backend selection: `agent: openclaw`
- supervisor enablement: explicit OpenClaw supervision config or provider selection
- authority tier: advisory versus limited automation

Everything else should be derived from defaults or remain internal.

Recommended release labels:

- Phase 1: experimental backend
- Phase 2: experimental supervisor
- Phase 3: pilot automation
- Phase 4: beta

Avoid calling the integration "stable" until mixed-team pilots and rollback drills are complete.

## Migration Plan for Active Teams

### Principle

Do not migrate an actively-used Batty team by replacing multiple control surfaces at once.

### Safe migration order

1. Land the adapter with supervision disabled.
2. Run a non-critical engineer or sidecar team on OpenClaw backend only.
3. Enable supervisor MVP in advisory mode for a pilot team.
4. Enable limited automation for one escalation category at a time.
5. Expand to more teams only after telemetry shows clear value.

### Recommended pilot profile

Use a team that:

- already has stable Batty telemetry
- has a human manager actively watching early runs
- does not carry the highest-risk production delivery load
- exercises real dispatch/review/restart paths often enough to produce useful signal

### Compatibility strategy

- keep existing Claude/Codex/Kiro paths unchanged
- do not change default backend selection
- do not change default escalation authority
- ensure OpenClaw-specific config is optional and isolated

### Rollback plan

Rollback must be one config edit plus restart:

- switch affected members back to an existing backend
- disable OpenClaw supervision provider
- keep Batty's standard escalation path active

No migration step should require board rewrites, task-format rewrites, or history conversion.

## Risk Plan

### Primary risks

| Risk | Why it matters | Mitigation |
| --- | --- | --- |
| Contract ambiguity | Supervisor output becomes hard to parse or trust | Use a narrow machine-readable response schema; fail closed |
| Operator confusion | Too many knobs or unclear ownership | Limit visible config surface; document authority tiers clearly |
| Automation noise | New escalations create more work than they remove | Start advisory-only; add cooldowns and dedupe before action mode |
| Backend drift | OpenClaw CLI behavior changes and breaks adapter assumptions | Pin validated versions for rollout; add health/preflight checks |
| Hidden regressions | Non-OpenClaw teams accidentally inherit new behavior | Keep feature off by default and cover negative-path tests |
| Active-team disruption | Migration changes too many runtime assumptions at once | Pilot on one team/member first and preserve one-step rollback |

### Release gates

Do not expand rollout if any of these are true:

- malformed supervisor output rate is above a small agreed threshold
- operator escalations increase without faster recovery
- restart loops or duplicate actions appear in pilot runs
- support/debug time for the integration exceeds the time it saves

## Release / Rollout Checklist

### Before Phase 1 ships

- define the adapter contract and supported OpenClaw version
- add adapter unit tests
- add config validation coverage
- document enable/disable procedure

### Before Phase 2 pilot

- freeze the supervisor request/response schema
- add timeout, malformed-output, and unavailable-backend tests
- verify telemetry captures supervisor call count, outcome, and fallback reason
- verify disabled teams show zero behavior change
- prepare rollback instructions for operators

### Before Phase 3 pilot automation

- define authority tiers and defaults
- add cooldown and dedupe protections
- prove automated actions are reversible
- review escalation messages for operator clarity
- confirm manager/operator can distinguish "recommended" from "acted"

### Before broader beta rollout

- run at least one live pilot long enough to capture normal failures
- review metrics against baseline
- update docs and troubleshooting
- confirm rollback works in practice, not just on paper
- publish release notes with known limitations and deferred work

## Success Metrics

The integration is helping only if it improves outcomes, not if it merely produces more AI activity.

### Primary success metrics

- lower mean time to recover from ambiguous prompt or supervision-required states
- lower manual intervention count per active team-hour
- lower repeated escalation count for the same failure signature
- stable or improved task throughput and merge completion time on pilot teams
- no material regression in daemon stability, restart behavior, or board integrity

### Guardrail metrics

- malformed supervisor response rate
- supervisor timeout rate
- percent of supervisor decisions that fall back to manual handling
- duplicate or noisy escalation rate
- operator rollback rate after enabling OpenClaw supervision

### Adoption metrics

- number of teams running OpenClaw backend only
- number of teams running OpenClaw supervisor in advisory mode
- number of teams running limited automation
- percentage of teams that stay enabled after the pilot window

### Recommended success thresholds

Treat the rollout as successful enough to expand when pilot teams show:

- meaningful reduction in manual supervision work
- no significant increase in false escalations
- clear operator understanding of what OpenClaw did and why
- at least one rollback rehearsal completed successfully

Exact numeric thresholds should be set once the pilot baseline is measured from Batty telemetry, rather than guessed up front.

## Suggested Implementation Order

1. Add `OpenClawAdapter` and register it behind existing adapter lookup.
2. Add preflight/health checks and config validation.
3. Write the supervision schema and parser with strict validation.
4. Add advisory-only supervisor calls and telemetry.
5. Pilot on one member or one low-risk team.
6. Add limited automation for one escalation path.
7. Expand docs, dashboards, and release guidance.

This sequence keeps the riskiest step, automation, until after the contract and operator model are proven in real runs.
