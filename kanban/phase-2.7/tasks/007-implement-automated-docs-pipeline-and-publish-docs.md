---
id: 7
title: Implement automated docs pipeline and publish docs website
status: done
priority: high
created: 2026-02-22T00:50:41.491560661-05:00
updated: 2026-02-22T01:00:16.337247094-05:00
started: 2026-02-22T00:52:24.586830143-05:00
completed: 2026-02-22T01:00:16.337246693-05:00
tags:
    - docs
    - website
    - automation
    - ci
claimed_by: cape-staff
claimed_at: 2026-02-22T01:00:16.337247044-05:00
class: standard
---

Build a first-class documentation system for Batty that auto-generates reference docs, validates docs quality in CI, and publishes a navigable docs site.

## Context

We want Batty docs to be maintainable and trustworthy as the project evolves. OpenClaw's docs workflow is a useful benchmark:

- In-repo docs source under `docs/`
- Static docs site with structured config/navigation
- CI docs checks (`format`, `markdownlint`, broken-link audit)
- Docs-only change detection so heavy non-doc CI jobs can be skipped

## Goal

Ship a production-ready docs workflow where docs are generated from code where possible, validated on every docs change, and deployed automatically.

## Deliverables

1. **Docs site scaffold**
- Add docs site framework and base structure under `docs/`.
- Define navigation, theme, and landing page.
- Add top-level sections: Getting Started, CLI Reference, Configuration, Architecture, Troubleshooting.

2. **Auto-generated reference docs**
- Generate CLI command reference from the clap command tree.
- Generate configuration reference from Rust config defaults and documented fields.
- Write generated outputs to deterministic paths (for example `docs/reference/cli.md` and `docs/reference/config.md`).
- Add a regeneration command/script that can run locally and in CI.

3. **Docs quality gates in CI**
- Markdown format check.
- Markdown lint check.
- Internal link audit (fail CI on broken internal links).
- Ensure generated docs are up to date (regenerate in CI and fail on diff).

4. **Docs-only change optimization (optional but strongly preferred)**
- Add a CI scope detector that marks docs-only changes.
- Skip heavy non-doc jobs when a PR only changes docs.

5. **Publishing workflow**
- Add deployment workflow for docs on merge to `main`.
- Support preview links for PRs if the chosen docs platform supports it.
- Document required secrets/variables for deployment.

6. **Contributor workflow docs**
- Add `docs/README.md` (or equivalent) with:
  - How to run docs locally
  - How to regenerate reference docs
  - How CI validates docs
  - Common troubleshooting steps

## Suggested Approach

- Prefer an off-the-shelf docs stack (Mintlify, MkDocs Material, or Docusaurus) over custom implementation.
- Keep doc generation deterministic and idempotent.
- Treat docs generation/linting as a required quality gate, not best-effort.

## Non-goals

- Rewriting all existing planning/kanban content into docs in this task.
- Perfecting the entire information architecture for all future features.
- Building a custom docs engine from scratch.

## Acceptance Criteria

1. `docs` site runs locally and contains the required core sections.
2. CLI and config reference pages are generated from source and checked into git.
3. CI enforces formatting, linting, link checks, and generated-doc freshness.
4. Docs publish automatically from `main`.
5. Contributor instructions for docs authoring/generation are committed.
6. Existing core workflows continue passing (`batty work`, `batty resume`, `batty board`) with no regressions.

## Verification

1. Run docs locally (framework-specific command) and confirm site renders.
2. Run docs generation and confirm deterministic output (no diff on second run).
3. Run docs checks locally (`format`, lint, link audit) and confirm pass.
4. Open a docs-only PR and confirm scoped CI behavior.
5. Merge to `main` and confirm docs deployment succeeds.

## Statement of Work

- **What was done:** Implemented a MkDocs-based documentation site with generated CLI/config references, CI docs quality gates, docs-only CI optimization, and GitHub Pages publishing workflow with PR preview artifacts.
- **Files created:**
  - `.github/workflows/docs-publish.yml` - build and publish docs on `main`, upload PR preview artifacts.
  - `docs/index.md` - docs landing page.
  - `docs/getting-started.md` - operator setup and runtime command guide.
  - `docs/architecture.md` - architecture overview and source links.
  - `docs/troubleshooting.md` - operational troubleshooting guide.
  - `docs/README.md` - contributor docs workflow and CI behavior.
  - `docs/reference/cli.md` - generated CLI command reference.
  - `docs/reference/config.md` - generated configuration reference.
  - `scripts/generate-docs.sh` - deterministic docs generation entrypoint for local/CI.
  - `src/bin/docsgen.rs` - Rust docs generator for CLI/config references.
- **Files modified:**
  - `.github/workflows/ci.yml` - added docs quality job and docs-only change optimization that skips heavy Rust matrix jobs for docs-only PRs.
  - `.gitignore` - ignore generated `site/` folder.
  - `Makefile` - added docs helper targets.
  - `mkdocs.yml` - configured Material site structure/navigation and strict mode.
- **Key decisions:**
  - Used MkDocs Material for low-risk, maintainable static docs.
  - Generated references from clap command tree and `ProjectConfig::default()` to keep docs deterministic and source-backed.
  - Used `mkdocs build --strict` as internal-link quality gate.
  - Used GitHub Pages for publish and workflow artifacts for PR previews (no extra secrets required).
- **How to verify:**
  - `./scripts/generate-docs.sh && git diff --exit-code -- docs/reference/cli.md docs/reference/config.md`
  - `cargo test`
  - CI docs job runs `mdformat`, `markdownlint-cli2`, `mkdocs build --strict`, and generated-doc freshness check.
- **Open issues:**
  - Local mkdocs/markdownlint checks were not runnable in this environment due missing `mkdocs` and network-restricted npm access; these checks are enforced in CI.
