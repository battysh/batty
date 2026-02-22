---
id: 5
title: tmux capability and version compatibility
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T23:37:56.754267013-05:00
started: 2026-02-21T23:34:03.84680201-05:00
completed: 2026-02-21T23:37:56.754266663-05:00
tags:
    - core
    - tmux
claimed_by: oaken-south
claimed_at: 2026-02-21T23:37:56.754266963-05:00
class: standard
---

tmux behaviors differ by version. Batty should probe capabilities and use compatible command paths.

## Requirements

1. Detect tmux version on startup and log it.
2. Probe required capabilities:
   - `pipe-pane` behavior
   - status bar formatting options used by Batty
   - pane split behavior used for orchestrator log pane
3. Provide compatibility matrix in docs with known-good version range.
4. Fail fast with clear remediation when required capabilities are missing.
5. Use fallbacks when possible instead of hard failure.

## Deliverables

- Capability probe module and tests.
- User-facing compatibility section in docs.

## Statement of Work

- **What was done:** Added tmux capability probing at orchestrator startup with required-capability gating and compatibility fallbacks for non-critical features.
- **Files created:** None.
- **Files modified:** `src/tmux.rs` - added capability probe model (`TmuxCapabilities`), version parsing, probe execution, split mode selection, and supporting helpers/tests; `src/orchestrator.rs` - startup probe integration, fail-fast remediation on missing required `pipe-pane`, and fallback-aware pipe/log-pane behavior; `README.md` - added tmux compatibility matrix and runtime probe behavior docs.
- **Key decisions:** Treated `pipe-pane` as required (hard fail) while keeping status styling and log-pane split behavior as fallback-enabled; selected split strategy (`-l` vs `-p`) from probe results to avoid hard failures on older tmux builds.
- **How to verify:** `cargo test -q` (full suite); representative focused checks: `cargo test -q tmux::tests::capability_probe_reports_pipe_pane`, `cargo test -q tmux::tests::parse_tmux_version_supports_minor_suffixes`.
- **Open issues:** Compatibility matrix currently documents `>=3.2` as known-good and `3.1.x` as fallback-supported; expand matrix with additional explicitly-tested versions as dogfooding broadens.
