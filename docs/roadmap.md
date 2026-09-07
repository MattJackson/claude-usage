# Roadmap & branch model

## Branches

- **`release/0.4.x`** — maintenance line, pinned at v0.4.1. Hotfixes only.
  Every bug found in prod lands here first, gets a `0.4.x` tag, brew ships
  it. Then the same commit is merged into `dev`.
- **`main`** — the currently-released version. Fast-forwards to whatever
  the newest tagged release is (currently v0.4.1 == release/0.4.x tip).
- **`dev`** — v0.5.0 line. Features + refactors. Bugs cherry-picked from
  release/0.4.x on merge.

## v0.5.0 scope (feature line)

### Menu-bar redesign (multi-provider prep)

Now that Codex is coming and 13 more provider slots exist, the current
top-of-menu status header ("Session resets in X, Weekly resets in X,
Estimate 1h 27m") doesn't scale — one such block per provider would fill
the screen. Consolidate:

- **Remove** the two big status rows at the top of the menu.
- **Main menu** = flat "Claude — account | account | ..." per provider,
  one line per account. Provider label on the left, account list next
  to it, active account marked (▶ or bold).
- **Weekly summary** moves to a submenu ("Weekly overview").
- **Side menu** = white text, non-clickable (currently grey/disabled and
  hard to read).
- **Side menu** = **no percentages** (already shown in main), just
  labels/timings/context.

### API-key providers

- **Remove "Paste API Key"** action entry from the "Capture current login"
  submenu. Instead, just **list the API-key providers below the OAuth
  ones** in the main menu, same as any captured account.

### Codex switching

Currently reporting-only for Codex. In v0.5.0 the switch/start/continue
verbs work for Codex too.

### Platforms

Linux + Windows platform impls (drafts staged at /tmp/usagio-drafts/).

## v0.5.0 quality bar

Same as v0.4.0: `/codeaudit` converges to 0 confirmed issues, all lenses
retired, 357/357 tests green under `--test-threads=1` × 5 runs.

## Hotfix candidates (→ release/0.4.x → 0.4.2)

Filed by user during 0.4.1 soak — evaluating each for 0.4.x vs. 0.5.0:

- **Refuse `usagio switch <locked-account>`.** UX bug: switching to an
  account that's at 100% capacity silently succeeds and then Claude
  Code errors on first request. Should refuse with a helpful message.
  → **0.4.x hotfix.**
- **Side menu contrast (grey → white).** Currently uses NSMenu default
  disabled colour which is illegibly grey on modern macOS. Ship on
  0.4.x as a readability fix; the more ambitious "no percentages"
  restructure ships with 0.5.0's redesign.
  → **0.4.x hotfix (colour only).**

Everything else on this page is 0.5.0 material.
