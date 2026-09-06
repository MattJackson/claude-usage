# Security Policy

`claude-usage` handles **Claude OAuth tokens** — it stores them in the macOS
Keychain and in `~/.config/claude-usage/state.json` (owner-only, `0600`), and it
talks only to first-party Anthropic/Claude endpoints. Its hard requirement is that
credentials stay local, stay owner-readable only, and are sent nowhere but the
official endpoints.

Classes of bug that qualify as security issues:

- Token leakage: a token (access or refresh) written to logs, stdout, error
  output, or otherwise exposed.
- World-readable or group-readable token storage — anything that leaves
  `state.json` (or another credential file) with permissions wider than `0600`.
- A code path that sends credentials anywhere other than the official
  Anthropic/Claude endpoints.
- Privilege issues in the `launchd` daemon (e.g. running with more privilege than
  needed, or a way for another local user/process to influence what it does).

## Supported versions

| Version | Supported |
|---|---|
| `main` branch | ✅ |
| latest tagged release | ✅ |

## Reporting a vulnerability

Please report privately to **matthew@pq.io** (or open a GitHub Security Advisory on
the repository). Include a minimal reproducer and the observed behavior. Please do
not open a public issue for a token-exposure or credential-leak bug until it is
fixed.
