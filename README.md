# claude-usage

[![CI](https://github.com/MattJackson/claude-usage/actions/workflows/ci.yml/badge.svg)](https://github.com/MattJackson/claude-usage/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/MattJackson/claude-usage?display_name=tag&sort=semver)](https://github.com/MattJackson/claude-usage/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

See your Claude usage across multiple **Claude Max** accounts, switch between
them instantly without the `/login` browser dance, and (optionally) auto-ride
each account's weekly quota to ~95% without ever slamming into the 100% wall
that interrupts your work.

> **Unofficial.** This project is not affiliated with, endorsed by, or supported
> by Anthropic. It talks only to the same first-party endpoints the official
> Claude Code CLI uses, with the same public OAuth client id. Use at your own
> risk. **macOS only** (it relies on the macOS Keychain and `launchd`).

**At a glance**

| | |
|---|---|
| **Platform** | macOS |
| **Language** | Rust (single binary, no webview) |
| **Talks to** | `api.anthropic.com` / `platform.claude.com` only |
| **Token storage** | `~/.config/claude-usage/state.json` (chmod 600) |

## Table of contents

- [Why](#why)
- [Install](#install)
- [Onboarding accounts](#onboarding-accounts)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Auto-swap daemon](#auto-swap-daemon)
- [Menu bar app](#menu-bar-app)
- [How it works](#how-it-works)
- [Security](#security)
- [License](#license)

## Why

If you run more than one Claude Max subscription, there is no built-in way to:

- see, at a glance, how much of each account's **session (5h)** and **weekly
  (7d)** limits you've used and when they reset;
- switch which account `claude` uses without going through the interactive
  `/login` browser flow every time; or
- keep working when one account hits its limit — normally you just get blocked.

`claude-usage` solves all three.

## Install

### Homebrew (recommended)

```sh
brew install MattJackson/tap/claude-usage
claude-usage install     # menu bar + auto-swap, now and at every login
```

### From source

Requires a recent Rust toolchain.

```sh
git clone https://github.com/MattJackson/claude-usage.git
cd claude-usage
cargo build --release
# binary at target/release/claude-usage — copy it onto your PATH:
cp target/release/claude-usage /usr/local/bin/
```

## Onboarding accounts

`claude-usage` onboards accounts by **capture**: you log into each account the
normal way once, and it snapshots that login into its own store. After that it can
switch between them freely.

**Why capture?** macOS holds exactly one Claude login in the Keychain at a time. So
you log into an account, capture it (copying its credentials into
`~/.config/claude-usage/state.json`), then log into the next one and capture that.
Once all are captured, switching just rewrites that single Keychain item — no more
logins needed.

```sh
# 1. Log into your first account, then capture it.
claude                      # /login as account A in the browser, back to the prompt
claude-usage capture work   # snapshots it, names it "work"  ->  Captured 'work' (you@work.com)

# 2. Log into your second account (this replaces the Keychain), then capture it.
claude                      # /login as account B
claude-usage capture personal

# 3. Confirm both are onboarded.
claude-usage                # dashboard shows both; ▶ marks the active one
```

Add more accounts anytime by repeating with a new name. From the **menu-bar app**
you can do the same without the terminal: `claude` → `/login` as the new account,
then click the icon → **Capture current login…** → type a name.

Notes:

- You log in **once per account** — after capture, `claude-usage` refreshes each
  account's token itself, so you're never sent back through `/login`.
- The **active** account's tokens rotate as you use `claude`; the tool re-syncs it
  from the Keychain automatically so nothing goes stale.
- Remove an account with `claude-usage rm <name>`.

## Quick start

```sh
# 1. Log into your first account the normal way, then capture it:
claude            # /login as account A, then quit
claude-usage capture work

# 2. Log into another account, then capture that one too:
claude            # /login as account B, then quit
claude-usage capture personal

# 3. See everything:
claude-usage

# 4. Switch the active login (all running claudes pick it up):
claude-usage switch personal
```

Example dashboard:

```
    ACCOUNT      EMAIL                      SESSION (5h)           RESETS IN     WEEKLY (7d)            RESETS IN
    -------------------------------------------------------------------------------------------------------------
 ▶  work         you@work.com               [#---------]   9%      3h 12m       [----------]   2%      3d 4h
    personal     you@personal.com           [######----]  61%      1h 40m       [########--]  78%      2d 9h
```

## Commands

```
claude-usage                   Show usage for every account (default)
claude-usage capture <name>    Save the account you're currently logged into
claude-usage switch [name]     Make <name> the active login (no launch)
claude-usage start [name]      Switch, then launch a fresh `claude`
claude-usage continue [name]   Switch, then launch `claude --continue`
claude-usage token <name>      Print a fresh access token (for scripting)
claude-usage menubar           Menu-bar app (usage at a glance + switch + auto-swap)
claude-usage watch             Headless auto-swap at 95% (foreground, no menu bar)
claude-usage install           Run the menu-bar app at every login (via launchd)
claude-usage uninstall         Stop running the menu-bar app at login
claude-usage report            Usage patterns by weekday / hour / account
claude-usage rm <name>         Forget an account
```

With no `[name]`, `switch` / `start` / `continue` **auto-pick** the account that
has room and whose weekly limit resets soonest — so you burn quota that's about
to reset anyway.

```sh
claude-usage start             # auto-pick the best account and open claude
claude-usage continue work     # switch to "work" and resume your last conversation
CLAUDE_TOKEN=$(claude-usage token work)   # a fresh access token for scripts
```

## Auto-swap daemon

`watch` polls the active account and, when its session **or** weekly usage hits
**95%**, switches to another account that has room — so a long coding session
keeps going instead of hitting a wall. It uses hysteresis to avoid thrashing:

- **trigger** at 95% utilization, but only swap **to** an account at **≤85%**;
- a **swap cooldown** so it can't flip rapidly; and
- **no bounce-back** to an account it just left.

If no account has room, it doesn't swap — it fires a notification telling you the
soonest reset. The daemon runs inside the menu-bar app, so `claude-usage install`
(above) is the usual way to keep it always-on. For a headless machine with no menu
bar, run the loop directly in the foreground:

```sh
claude-usage watch       # headless, no menu bar
```

Tunables: `claude-usage watch --interval <secs> --trigger <pct> --ceiling <pct>`.

## Menu bar app

`claude-usage menubar` runs a macOS status-bar item that shows your current usage
at a glance (a live % in the menu bar), lets you switch accounts from a dropdown,
toggle auto-swap and its threshold, capture a new login, and start at login — all
while running the auto-swap daemon in the same process.

The simplest setup is:

```sh
brew install MattJackson/tap/claude-usage
claude-usage install     # runs the menu-bar app now and at every login
```

`claude-usage install` registers a `launchd` agent that launches `menubar` at
login; **Quit** in the menu stops it until the next login, and `claude-usage
uninstall` removes it entirely.

## How it works

- **Usage** comes from the same endpoint the `/usage` command uses,
  `GET https://api.anthropic.com/api/oauth/usage`, authenticated with your
  account's OAuth access token. It returns the 5-hour and 7-day windows with
  utilization percentages and reset timestamps.
- **Switching** writes the chosen account's login into the shared macOS Keychain
  item (`Claude Code-credentials`) — exactly what a real `/login` persists.
  Running `claude` processes re-read that credential, so they adopt the new
  account on their next request. No browser flow, no restart required.
- **Token freshness** is handled automatically: access tokens are refreshed via
  `platform.claude.com/v1/oauth/token` before they expire, and the currently
  active account is re-synced from the Keychain so token rotation done by live
  sessions is never lost.

## Security

- Tokens are stored locally in `~/.config/claude-usage/state.json` with `0600`
  (owner-only) permissions.
- The tool uses the **same public OAuth client id** as the official Claude Code
  CLI. It sends your tokens only to Anthropic's own endpoints
  (`api.anthropic.com`, `platform.claude.com`) — nowhere else.
- Switching writes to the same Keychain item Claude Code already uses; nothing
  leaves your machine.

## License

MIT — see [LICENSE](LICENSE).
