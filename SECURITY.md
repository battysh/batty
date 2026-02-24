# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Batty, please report it responsibly.

**Email:** Open a [private security advisory](https://github.com/battysh/batty/security/advisories/new) on GitHub.

Please include:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge receipt within 48 hours and aim to release a fix within 7 days for critical issues.

## Scope

Batty executes shell commands and spawns agent processes in tmux sessions. Security-relevant areas include:

- **Command injection** via task descriptions or board content
- **Environment variable leakage** between sessions
- **Privilege escalation** through agent policy tiers
- **Sensitive data exposure** in execution logs

## Trust Model

Batty trusts:
- The kanban board content (task descriptions are passed to agents as prompts)
- The configured agent CLI (Claude Code, Codex, etc.)
- The tmux environment

Batty does NOT:
- Sanitize task descriptions for shell injection (boards are author-controlled)
- Encrypt execution logs (they may contain agent output with sensitive data)
- Restrict agent filesystem access (that's the agent CLI's responsibility)

## Out of Scope

- Vulnerabilities in upstream agent CLIs (Claude Code, Codex)
- Issues requiring physical access to the machine
- Social engineering attacks
- Denial of service via resource exhaustion (Batty is a local dev tool)

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes      |
