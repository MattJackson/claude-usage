# usagio v0.5.0 — test plan: validating the never-re-login contract

Audience: Matthew, running this by hand on macOS today, and on Linux/Windows
once those machines are available. For the design these tests are checking,
see `docs/architecture-0.5.0.md`.

The CAS active-refresh work has landed on `dev`/`feat/v0.5.0` — every test
below, including the CAS-specific ones (T3, T6), is grounded in merged code
and the log event names/fields shown are the actual ones emitted by
`src/main.rs`.

## Prerequisites

Run these before starting any test below — if any fails, stop and fix it
first, the tests downstream assume a working v0.5.0 install.

```sh
usagio --version                       # → usagio 0.5.0
pgrep -f 'usagio menubar'              # → a PID (menu-bar app is running)
ls ~/.config/usagio                    # → state.json, usagio.log, backups/
```

If `pgrep` prints nothing, the menu-bar app isn't running — `usagio menubar`
starts it in the foreground, or check `usagio doctor` / the launchd/systemd
autostart status (Settings menu on macOS) to see why the background instance
isn't up.

The log file is `~/.config/usagio/usagio.log`. Every test below is checked
primarily by `tail -f` / `grep` against this file, since usagio never shows
its own toast for most of these transitions by design (that would defeat the
"never re-login" point — it should just work silently).

---

## Contract tests

These are non-negotiable for shipping 0.5.0. If any fails, do not ship —
follow the symptom table below to localize the cause, or fall back to the
escape hatch.

### T1 — Capture

1. In Claude Code, run `/login` and complete auth for an account you don't
   already have captured.
2. Run `usagio capture`.
3. Verify:
   - `~/.config/usagio/state.json` gained (or refreshed) an entry for that
     account's email, and `active` in state.json now names it.
   - The log contains a structured `event=capture account=<email>
     existed=<true|false> at_prefix=<...> ...` line, in addition to the
     human-readable stdout confirmation (`Captured <email> — it's the active
     login.` / `Refreshed <email> — it's the active login.`).

### T2 — Never-re-login soak (24h)

1. Capture N ≥ 2 accounts (`usagio capture` once per account, per T1). Note
   each account's `expires_at` from `usagio list` or state.json.
2. Leave the machine running normally (menu-bar app up, Claude Code used as
   usual) for 24 hours. Don't manually intervene.
3. After 24h, verify:
   - **No `/login` prompt** appeared in Claude Code at any point — this is
     the whole point of the release. Any `/login` prompt during the soak is
     an automatic fail; go straight to the symptom table.
   - **`expires_at` rolled forward at least once** for every account,
     active and inactive alike (a session token's default lifetime is well
     under 24h, so each account should show multiple rotations over the
     window).
   - **The keychain slot for the active account matches state.json** — on
     macOS: `security find-generic-password -s "Claude Code-credentials" -w`
     should decode to the same account (by email, via
     `~/.claude.json`'s `oauthAccount`) that `state.json`'s `active` names,
     and the same access token that `usagio list --refresh` last fetched
     usage with.

### T3 — Active-account CAS refresh (the v0.5.0 design)

This is the test that exercises the actual mechanism the release is named
for — everything else is either supporting infrastructure or unaffected by
the change.

1. Get an account into the "active" slot with its token near expiry (either
   wait for a natural near-expiry moment, or use a short-lived test account
   if one exists).
2. Start (or keep running) Claude Code on that account.
3. Watch `~/.config/usagio/usagio.log` for one of:
   - `event=active_refresh_cas_won`
   - `event=active_refresh_cas_lost`
   - `event=active_refresh_skipped_drift`
4. **All three are correct outcomes.** They represent, respectively: usagio
   refreshed the token itself and won the race; usagio saw the vendor CLI
   had already rotated it and adopted that instead; usagio detected
   in-flight drift and safely deferred to the next cycle. None of the three
   should produce a `/login` prompt in Claude Code — that's the only actual
   failure condition for T3. If you see none of the three events at all
   after the account visibly approached and crossed a normal expiry
   boundary, that's the failure signal (the CAS cycle silently isn't
   running) — check the symptom table.

### T4 — Switch

1. Run `usagio switch <email>` for an account other than the currently
   active one.
2. Verify:
   - **No keychain dialog** appears (no "usagio wants to use your
     confidential information stored in..." prompt).
   - A fresh Claude Code session (quit and relaunch, or a new terminal tab
     running `claude`) uses the new account — confirm via `/status` in
     Claude Code or by checking which account's usage ticks up.
   - Log line: `event=switch from=<old-email> to=<new-email>
     identity_written=ok keychain_written=ok` — confirm both
     `identity_written` and `keychain_written` read `ok`, not just that a
     switch line exists at all.

### T5 — Auto-swap

1. Set (or wait for) an account to reach ≥95% (or whatever `trigger_pct` is
   configured to in Settings ▸ Auto-swap threshold; default options are
   90/95/98).
2. Verify:
   - Auto-swap fires within the adaptive cadence window: once peak session
     usage across accounts is at or above the trigger, the poll loop should
     already be at its 10-second "backstop" cadence (tightened from the
     150s base once usage crossed `trigger − 15` points, per the adaptive
     cadence design) and should have swapped within roughly 10 seconds of
     crossing the trigger. Confirm via the `cadence: <old>s → <new>s (max
     session ...%, trigger ...%)` log lines showing the tightening, followed
     by a swap-related log line.
   - **No keychain dialog** appears during the swap.

### T6 — Inactive-account refresh (mirror-back)

1. Pick an account that is currently inactive and near token expiry.
2. Wait for the next poll cycle (base cadence is 150s; shorter if any
   account is near its auto-swap trigger — see T5).
3. Verify the log shows:
   `event=inactive_refresh account=<email> at_prefix=<old-token-prefix> ->
   <new-token-prefix> mirror_back=ok`
   confirming both that usagio refreshed the token server-side AND mirrored
   the result back to wherever the vendor CLI would read it
   (`Provider::mirror_rotated_token` — keychain on macOS for Claude, plaintext
   `~/.claude/.credentials.json` on Linux/Windows; `auth.json` for Codex).
   `mirror_back=ok` (rather than a missing field, or `mirror_back=err`) is
   the pass condition — a refresh that updates `state.json` but fails to
   mirror is a latent "re-login on switch" bug even though this cycle looks
   fine.

### T7 — Backups Save/Restore

1. Menu bar ▸ Settings ▸ Advanced ▸ Backups ▸ **Save…** — a native macOS
   save panel should appear (not usagio's own dialog). Save to a temp
   location.
2. Menu bar ▸ Settings ▸ Advanced ▸ Backups ▸ **Restore…** — a native open
   panel should appear, defaulting into `~/.config/usagio/backups/` (the
   automatic rolling-backup directory usagio writes to on every real
   `state.json` save). Pick the file saved in step 1 (or a rolling backup)
   and restore it.
3. Verify:
   - State after restore matches what was saved (accounts, active selection).
   - No crash, no keychain dialog.
   - A safety copy of the pre-restore live state was itself stashed to
     `~/.config/usagio/backups/` before the restore overwrote it (so a bad
     restore choice is itself recoverable).

---

## Symptom → diagnosis table

| Log pattern (grep this) | Likely root cause |
|---|---|
| Any `/login` prompt in Claude Code during T2/T3/T4/T5 | Never-re-login contract broken — CAS lost track of the active token, or a mirror-back (T6) didn't happen before a switch (T4) made that account active. Highest-priority bug; do not ship. |
| `credentials: inactive refresh for <email> failed: ...` | Inactive-account refresh hit a network/API error (not `invalid_grant`) — check connectivity, check the vendor's OAuth endpoint status. Not itself fatal to the invariant; a later cycle should retry. |
| `flag_needs_relogin` / `needs_relogin=true` in state.json for an account you didn't expect | That account's refresh token was invalidated — either it really did need re-login (expected after a long-dormant account or manual revoke), or a race let something else consume the refresh token first. If it happened to an account you were actively using, that's T3/CAS failing to protect the active slot. |
| `menubar poll failed: ...` | The poll cycle itself errored (not a single account's refresh) — check `state.json` isn't corrupt, check disk permissions on `~/.config/usagio`. |
| `rate limited; backing off to <n>s` | Expected under heavy polling / after a burst of manual `--refresh` calls; exponential backoff, not a bug. Should recover once the vendor's rate limit window passes. |
| No `cadence:` tightening lines ever appear despite an account visibly approaching the trigger | Adaptive cadence isn't engaging — check `TRIGGER_PCT` / `trigger_pct` in state.json matches what Settings shows, and that `max_session_pct` is actually being computed (a stale usage cache would make usagio think usage is lower than it is). |
| A keychain "always allow?" / SecurityAgent dialog appears at all | The `security` CLI path isn't being used, or `-U` update-in-place regressed back in over delete-then-add — see `docs/architecture-0.5.0.md`'s CAS-mechanics section. |
| `security delete-generic-password` / `add-generic-password` failing repeatedly in the log | Keychain is locked (screen-lock timing?), or a permissions/ACL issue on the `"Claude Code-credentials"` item — check `security find-generic-password -s "Claude Code-credentials"` manually. |
| `event=active_refresh_skipped_drift` appearing every single cycle for the same account, never resolving to `_won`/`_lost` | CAS is stuck comparing against a stale baseline — likely a bug in the before-read/after-read comparison rather than genuine concurrent drift; file as a bug rather than assuming it's benign. |
| `mirror_back=err` (or the field missing entirely) in an `inactive_refresh` line | `mirror_rotated_token` failed or wasn't called for that provider — check the specific provider's override (Claude/Codex) is wired up, and that the target path (keychain / `.credentials.json` / `auth.json`) is writable. |

---

## Escape hatch — rollback to 0.4.3

If 0.5.0 fails the soak (T2) or the CAS test (T3) in a way that's actively
causing re-login loops, don't keep running it — roll back:

```sh
brew uninstall usagio
brew install usagio@0.4.3        # or: brew install <tap>/usagio --version 0.4.3
```

If no versioned formula is available, reinstall from the `release/0.4.x`
branch/tag directly (`git checkout release/0.4.x`, build, `cargo install
--path .`).

After rolling back:

1. `usagio capture` once per account to re-sync `state.json` against
   whatever the vendor CLI currently has in its keychain/file (0.4.3's
   capture is safe to re-run; it doesn't assume 0.5.0's state.json shape).
2. Confirm `usagio list` shows all accounts and none are flagged
   `needs_relogin` before resuming normal use.
3. File the failing test's log excerpt (the `event=...` lines around the
   failure, or lack thereof) before rolling back — that's the evidence the
   CAS PR needs to reproduce and fix the gap.
