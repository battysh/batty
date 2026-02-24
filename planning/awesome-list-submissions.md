# Awesome List Submissions for Batty

Draft PR descriptions for submitting Batty to curated awesome lists on GitHub.

**Project:** Batty -- Supervised agent execution for software teams. Kanban-driven, tmux-native, test-gated. Works with Claude Code, Codex, Aider. Built in Rust.
**GitHub URL:** https://github.com/battysh/batty
**License:** MIT (verify before submitting)

---

## 1. awesome-rust (rust-unofficial/awesome-rust)

**Repo:** https://github.com/rust-unofficial/awesome-rust
**Section:** `## Development tools` (line ~744 in README.md)
**Sort order:** Alphabetical -- entry goes between existing "b" entries (after `biome`, before `clippy`)

### Submission Requirements

- Project must have **50+ GitHub stars** OR **2000+ crates.io downloads** (or equivalent popularity metric specified in the PR).
- Entry format: `[ACCOUNT/REPO](https://github.com/ACCOUNT/REPO) [[CRATE](https://crates.io/crates/CRATE)] - DESCRIPTION`
- If not published on crates.io, omit the `[[CRATE](...)]` part.
- If CI badge exists (GitHub Actions), include it after the description.
- Alphabetical order within the section is required.
- Read full guidelines: https://github.com/rust-unofficial/awesome-rust/blob/main/CONTRIBUTING.md

### Exact Markdown Line

Without crates.io (if not yet published):

```markdown
* [battysh/batty](https://github.com/battysh/batty) - Supervised agent execution for software teams: kanban-driven, tmux-native, test-gated task dispatch for Claude Code, Codex, and Aider [![build badge](https://github.com/battysh/batty/actions/workflows/ci.yml/badge.svg)](https://github.com/battysh/batty/actions)
```

With crates.io (if published):

```markdown
* [battysh/batty](https://github.com/battysh/batty) [[batty](https://crates.io/crates/batty)] - Supervised agent execution for software teams: kanban-driven, tmux-native, test-gated task dispatch for Claude Code, Codex, and Aider [![build badge](https://github.com/battysh/batty/actions/workflows/ci.yml/badge.svg)](https://github.com/battysh/batty/actions)
```

### PR Title

```
Add battysh/batty to Development tools
```

### PR Notes

- Verify the CI badge URL matches the actual workflow file name in the repo.
- If the project has fewer than 50 stars at submission time, mention download counts or other traction metrics in the PR body.
- The list uses `*` bullets (not `-`).

---

## 2. awesome-cli-apps (agarrharr/awesome-cli-apps)

**Repo:** https://github.com/agarrharr/awesome-cli-apps
**Section:** `### Devops` (under the Development parent section)
**Sort order:** Add at the bottom of the Devops subsection

### Submission Requirements

- Software must be **free and open source**.
- Must be **older than 90 days** since first release.
- Must have **20+ GitHub stars**.
- Must be **easy to install** and **well documented**.
- Entry format: `[APP_NAME](LINK) - DESCRIPTION.`
- Description starts with a capital letter and ends with a full stop (period).
- Description should be short and concise. Do NOT include redundant words like "CLI" or "terminal".
- One PR per app submission.
- PR title must be exactly: `Add APP_NAME`
- Must use the provided PR template.
- Read full guidelines: https://github.com/agarrharr/awesome-cli-apps/blob/master/contributing.md

### Exact Markdown Line

```markdown
- [batty](https://github.com/battysh/batty) - Supervised agent execution for software teams with kanban-driven task dispatch.
```

### PR Title

```
Add batty
```

---

## 3. awesome-devops (wmariuss/awesome-devops)

**Repo:** https://github.com/wmariuss/awesome-devops
**Section:** `## Automation & Orchestration` (the closest fit -- covers deployment, provisioning, orchestration, and configuration management tools)
**Alternative section:** `## Productivity Tools` (if the maintainers prefer)
**Sort order:** Add at the bottom of the chosen section

### Submission Requirements

- Entry format: `[RESOURCE](LINK) - DESCRIPTION.`
- Description must be under 80 characters.
- Description ends with a full stop.
- One commit per category.
- PR title should use imperative form (e.g., "Add" not "Added" or "Adding").
- Include application name, category, and link to the open source project in the PR description.
- Read full guidelines: https://github.com/wmariuss/awesome-devops/blob/master/CONTRIBUTING.md

### Exact Markdown Line

For Automation & Orchestration:

```markdown
- [Batty](https://github.com/battysh/batty) - Supervised agent execution with kanban-driven, tmux-native task dispatch.
```

For Productivity Tools:

```markdown
- [Batty](https://github.com/battysh/batty) - Supervised agent execution for software teams with test-gated task dispatch.
```

### PR Title

```
Add Batty
```

### PR Body

```
Adds [Batty](https://github.com/battysh/batty) to the Automation & Orchestration section.

Batty is a hierarchical agent command system for software development. It reads
a kanban board, dispatches tasks to coding agents (Claude Code, Codex, Aider),
supervises their work via tmux, gates on tests, and merges results. Built in Rust.
```

---

## 4. awesome-selfhosted (awesome-selfhosted/awesome-selfhosted)

**Repo:** https://github.com/awesome-selfhosted/awesome-selfhosted
**Data Repo:** https://github.com/awesome-selfhosted/awesome-selfhosted-data (PRs go HERE, not the main repo)
**Section/Tag:** `Software Development - Project Management`

### Submission Requirements

This list uses a **YAML-based data repo** -- you do NOT edit the README directly. Instead:

1. Create a new file at `software/batty.yml` in the **awesome-selfhosted-data** repo.
2. Follow the template from `.github/ISSUE_TEMPLATE/addition.md`.
3. Project must be **actively maintained**.
4. Project must have been **first released more than 4 months ago**.
5. Project must have **working installation instructions**.
6. Avoid redundant terms like "open-source", "free", "self-hosted" in the description.
7. Prefer shorter description forms (no leading "A" article).
8. Read full guidelines: https://github.com/awesome-selfhosted/awesome-selfhosted-data/blob/master/CONTRIBUTING.md

### YAML File Content (`software/batty.yml`)

```yaml
# software name
name: "Batty"
# URL of the software project's homepage
website_url: "https://github.com/battysh/batty"
# URL where the full source code of the program can be downloaded
source_code_url: "https://github.com/battysh/batty"
# description of what the software does, shorter than 250 characters, sentence case
description: "Hierarchical agent command system that reads a kanban board, dispatches tasks to coding agents, supervises via tmux, gates on tests, and merges results."
# list of license identifiers
licenses:
  - MIT
# list of languages/platforms
platforms:
  - Rust
# list of tags (categories)
tags:
  - Software Development - Project Management
```

### Rendered Entry (what it will look like)

```markdown
- [Batty](https://github.com/battysh/batty) - Hierarchical agent command system that reads a kanban board, dispatches tasks to coding agents, supervises via tmux, gates on tests, and merges results. ([Source Code](https://github.com/battysh/batty)) `MIT` `Rust`
```

### PR Title

```
Add Batty
```

### Notes

- The PR goes to `awesome-selfhosted/awesome-selfhosted-data`, NOT the main `awesome-selfhosted` repo.
- Verify the license SPDX identifier matches what is in the repo's LICENSE file.
- If Batty has a website or hosted demo, add `demo_url` to the YAML.

---

## 5. awesome-ai-agents (e2b-dev/awesome-ai-agents)

**Repo:** https://github.com/e2b-dev/awesome-ai-agents (26k+ stars)
**Section:** `# Open-source projects` (alphabetical order -- entry goes between entries starting with "B")
**Submission form:** https://forms.gle/UXQFCogLYrPFvfoUA (preferred method)

### Submission Requirements

- Can submit via **Pull Request** or via the **Google Form** linked above.
- Maintain alphabetical order within the section.
- Place in the correct category (Open Source Projects).
- Format follows the repo's specific HTML/Markdown hybrid style with collapsible details sections.
- Read the repo README for the latest submission instructions.

### Exact Markdown Block

```markdown
## [Batty](https://github.com/battysh/batty)
Supervised agent execution for software teams

<details>

### Category
Coding, Multi-agent, Build your own

### Description
- Hierarchical agent command system for software development
- Reads a kanban board, dispatches tasks to coding agents, supervises their work, gates on tests, and merges results
- Kanban-driven task management with Markdown-based state
- tmux-native supervision with PTY management
- Test-gated workflow -- all tests must pass before task completion
- Works with Claude Code, Codex, Aider, and other coding agents
- Parallel DAG scheduling for concurrent task execution
- Built in Rust for performance and reliability

### Links
- [GitHub](https://github.com/battysh/batty)
</details>
```

### PR Title

```
Add Batty - supervised agent execution for software teams
```

### Notes

- This list is curated by e2b.dev and is specifically for AI agents/assistants (not SDKs or frameworks).
- The form submission may be faster than a PR as the maintainers process submissions in batches.
- Batty fits well here because it orchestrates and supervises AI coding agents.

---

## Pre-Submission Checklist

Before submitting to any of these lists, ensure:

- [ ] The GitHub repo (https://github.com/battysh/batty) is public.
- [ ] The repo has a clear README with installation instructions.
- [ ] The LICENSE file exists and matches the SPDX identifier used in submissions.
- [ ] CI/CD badges are working and green.
- [ ] The project has sufficient GitHub stars for lists with minimum requirements (50 for awesome-rust, 20 for awesome-cli-apps).
- [ ] The project has been publicly available for 90+ days (required for awesome-cli-apps) or 4+ months (required for awesome-selfhosted).
- [ ] The crates.io package is published (if including crate badge for awesome-rust).

## Submission Priority

Recommended order based on audience fit and list requirements:

1. **awesome-ai-agents** -- Strongest fit. Batty is literally an AI agent orchestrator. Submit via the Google Form first for quick processing, then follow up with a PR.
2. **awesome-rust** -- Strong fit for Rust ecosystem visibility. Requires 50 stars or 2000 crate downloads.
3. **awesome-cli-apps** -- Good fit as a CLI development tool. Lower bar at 20 stars.
4. **awesome-devops** -- Reasonable fit under automation/orchestration. No star requirement mentioned.
5. **awesome-selfhosted** -- Weakest fit. Batty is a local dev tool, not typically "self-hosted" in the server sense. Consider skipping unless the project evolves to include a web dashboard or remote agent management.
