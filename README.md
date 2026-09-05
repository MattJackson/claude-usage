# claude-usage

[![CI](https://github.com/MattJackson/claude-usage/actions/workflows/ci.yml/badge.svg)](https://github.com/MattJackson/claude-usage/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/MattJackson/claude-usage?display_name=tag&sort=semver)](https://github.com/MattJackson/claude-usage/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

See your Claude usage across multiple **Claude Max** accounts from your menu bar,
switch between them without the `/login` browser dance, and (optionally) auto-ride
each account's quota to ~95% without ever slamming into the 100% wall that
interrupts your work.

> **Unofficial.** Not affiliated with, endorsed by, or supported by Anthropic. It
> talks only to the same first-party endpoints the official Claude Code CLI uses,
> with the same public OAuth client id. Use at your own risk. **macOS only** (it
> relies on the macOS Keychain and `launchd`).

## Install

```sh
brew install MattJackson/tap/claude-usage
claude-usage install     # menu-bar app + auto-swap, now and at every login
```

Upgrades come through Homebrew: `brew upgrade claude-usage` — a running menu-bar app
notices the new binary and relaunches itself into it, so you don't have to restart
anything. Building from source is covered [below](#from-source).

**At a glance**

| | |
|---|---|
| **Platform** | macOS |
| **Interface** | Menu-bar app (primary) + CLI |
| **Language** | Rust (single binary, no webview) |
| **Talks to** | `api.anthropic.com` / `platform.claude.com` only |
| **Token storage** | `~/.config/claude-usage/state.json` (chmod 600) + macOS Keychain |

## Table of contents

- [Why](#why)
- [Install](#install)
- [Onboarding accounts](#onboarding-accounts)
- [Menu bar app](#menu-bar-app)
- [CLI](#cli)
- [Auto-swap](#auto-swap)
- [How it works](#how-it-works)
- [Security](#security)
- [License](#license)

## Why

If you run more than one Claude Max subscription, there is no built-in way to:

- see, at a glance, how much of each account's **session (5h)** and **weekly (7d)**
  limits you've used and when they reset;
- switch which account `claude` uses without the interactive `/login` browser flow; or
- keep working when one account hits its limit — normally you just get blocked.

`claude-usage` solves all three. Accounts are identified by their **email** — there
are no nicknames to invent or keep in sync.

## From source

Requires a recent Rust toolchain.

```sh
git clone https://github.com/MattJackson/claude-usage.git
cd claude-usage
cargo build --release
cp target/release/claude-usage /usr/local/bin/
```

## Onboarding accounts

You log into each account once, the normal way, and `claude-usage` **captures** that
login — keyed by the account's email.

**Why capture?** macOS holds exactly one Claude login in the Keychain at a time. So
you log in as an account, capture it (its credentials are copied into
`~/.config/claude-usage/state.json`), then log in as the next one and capture that.
Once captured, switching just rewrites that single Keychain item — no more logins.

```sh
# 1. Log in as your first account, then capture it.
claude              # /login as account A, back to the prompt
claude-usage capture          #  ->  Captured you@work.com

# 2. Log in as your second account, then capture it.
claude              # /login as account B
claude-usage capture          #  ->  Captured you@personal.com

# 3. See everything.
claude-usage
```

From the **menu-bar app** you never need the terminal: `claude` → `/login` as the new
account, then click the icon → **Capture current login…**.

Notes:

- `capture` takes no name — it reads the email of whoever is currently logged in.
  Capturing an email that already exists just **refreshes** it; it never silently
  creates a duplicate or overwrites a different account.
- You log in **once per account**; after that `claude-usage` refreshes tokens itself.
- Remove an account from the menu, or with `claude-usage rm <email>`.

## Menu bar app

`claude-usage menubar` is the primary interface: a status-bar item showing your
active account's **session %** at a glance. Click it for a dropdown:

- **One submenu per account**, labelled `email` on the left with `S% / W%`
  right-aligned battery-menu style. The **active account is bold**; any percentage
  in a danger band is colored (amber ≥80%, red ≥95%) so a nearly-spent account
  jumps out. Each opens:
  - **Switch to this account**
  - a stats block — `Session X% · resets in …`, `Weekly X% · resets in …`, Opus if
    present, and `updated Xm ago`
  - **Remove…**
- **Auto-swap at high usage ▸ Off / 90% / 95% / 98%**
- **Capture current login…**
- **Launch at login** (toggle)
- **Quit**

The menu bar also runs the auto-swap daemon in the same process. `claude-usage
install` registers a `launchd` agent so it starts at every login (and runs
Dock-less — menu bar only). `claude-usage uninstall` removes it.

## CLI

Everything the menu does is scriptable. Accounts are selected by **email or a unique
prefix** (an ambiguous prefix lists the matches).

```
claude-usage                     Show usage for every account (default)
claude-usage capture             Capture the account you're currently logged into
claude-usage switch [acct]       Make <acct> the active login (no launch)
claude-usage start [acct]        Switch, then launch a fresh `claude`
claude-usage continue [acct]     Switch, then launch `claude --continue`
claude-usage token <acct>        Print a fresh access token (for scripting)
claude-usage rm <acct>           Forget an account
claude-usage menubar             Run the menu-bar app
claude-usage watch               Headless auto-swap (foreground, no menu bar)
claude-usage install|uninstall   Run / stop the menu-bar app at every login
claude-usage report              Usage patterns by weekday / hour / account
claude-usage --version
```

With no `[acct]`, `switch` / `start` / `continue` **auto-pick** the account that has
room and whose weekly limit resets soonest — so you burn quota that's about to reset
anyway.

```sh
claude-usage start               # auto-pick the best account and open claude
claude-usage continue dev        # prefix-match dev@…, switch, resume last conversation
TOKEN=$(claude-usage token dev)  # a fresh access token for scripts
```

## Auto-swap

When the active account's session **or** weekly usage crosses your threshold
(default **95%**, set from the menu), the daemon switches to another account that has
room — so a long session keeps going instead of hitting a wall. Hysteresis prevents
thrashing:

- trigger at the threshold, but only swap **to** an account with real headroom;
- a **swap cooldown** so it can't flip rapidly; and
- **no bounce-back** to an account it just left.

If no account has room it doesn't swap — it notifies you of the soonest reset. It
runs inside the menu-bar app (so `claude-usage install` keeps it on), or headless via
`claude-usage watch` on a machine with no menu bar.

## How it works

- **Usage** comes from the same endpoint the `/usage` command uses,
  `GET api.anthropic.com/api/oauth/usage`, authenticated with the account's OAuth
  token. Usage is fetched **only on the poll tick** and cached — `switch`, `list`,
  and the menu read the cache, so ordinary use never rate-limits you.
- **Switching** sets the active account by writing two things Claude Code reads: the
  OAuth token in the macOS Keychain item `Claude Code-credentials`, and the account
  identity (`oauthAccount`) in `~/.claude.json` — exactly what a real `/login`
  persists. The write is atomic and serialized across processes with a file lock.
  **A new `claude` session picks up the switched account; sessions already running
  keep their current account until they're restarted.**
- **Token freshness** is automatic: tokens are refreshed via
  `platform.claude.com/v1/oauth/token` before they expire, and the active account's
  tokens are re-synced from the Keychain (with an identity check, so a `/login` into
  a different account can't corrupt a stored one).

## Security

- Tokens live locally in `~/.config/claude-usage/state.json` (owner-only, `0600`)
  and the macOS Keychain, read/written via the Security framework (never passed on a
  command line).
- The tool uses the **same public OAuth client id** as the official Claude Code CLI,
  and sends tokens only to Anthropic's own endpoints — nowhere else.
- If `ANTHROPIC_API_KEY` is set in your environment, Claude Code uses that and
  ignores the Keychain login — so account switching won't take effect until it's
  unset. `claude-usage` manages subscription (OAuth) logins, not API keys.

## License

MIT — see [LICENSE](LICENSE).
