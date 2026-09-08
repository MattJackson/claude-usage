# usagio v0.5.0 architecture — never-re-login

One page. For the test plan that validates this design in the field, see
`docs/testing-0.5.0.md`.

## The invariant

> **`state.json` is authoritative for INACTIVE accounts. The vendor CLI's
> OS-native credential store is authoritative for the ACTIVE account.**

Pre-0.5.0, usagio treated every account's tokens the same way and would
proactively refresh them all, including whichever one the vendor CLI (e.g.
Claude Code) currently had open. That raced the vendor CLI's own rotation:
OAuth refresh tokens are single-use, so whichever side's `/token` POST lost
the race got an `invalid_grant` and, in the worst case, forced the user back
through `/login`. That regression is what "never re-login" means to close.

0.5.0 draws a hard line:

- **Inactive accounts** — usagio owns rotation outright. It refreshes them
  server-side on its own schedule and writes the result straight to
  `state.json`. The vendor CLI never touches these tokens (they're not the
  live login), so there is no race to lose.
- **The active account** — the vendor CLI (Claude Code) owns rotation. usagio
  does not blindly refresh it in the background (see `refresh_usage_cache`,
  which explicitly skips `ensure_fresh` for `state.active`). But usagio also
  doesn't just leave it alone forever — when the active account is near
  expiry, usagio runs a **compare-and-swap (CAS) rotation** against the
  vendor's OS-native store instead of an unconditional overwrite:

  1. **Read** the vendor's current credential blob (keychain on macOS, file
     on Linux/Windows).
  2. **Compare** it against the blob usagio last knew for that account.
     - **Match** → nobody else has rotated it since usagio last looked.
       usagio performs its own refresh POST and **writes back** the new
       blob (`active_refresh_cas_won`).
     - **Mismatch** → the vendor CLI (or another usagio process) already
       rotated it. usagio **adopts the drift** — records the on-disk blob
       as its new known-good baseline — and skips its own rotation this
       cycle (`active_refresh_cas_lost` if the drift was a completed
       rotation, `active_refresh_skipped_drift` if usagio can't yet tell
       what changed and defers to be safe).
  3. Either outcome ends with usagio's belief about the active account's
     tokens matching what's actually on disk — the invariant holds whether
     usagio won the race, lost it, or bowed out.

`[TBD: verify against CAS PR]` — the CAS active-refresh path (the
read/compare/write-back loop above, and the three `active_refresh_cas_*` log
events) is being built on a branch in flight at the time of writing; the
mechanics described here are the agreed design, not yet a merged
implementation. What *is* merged as of this writing is the first half: usagio
no longer touches the active account's tokens at all in the background
(`refresh_usage_cache` skips it outright). The CAS rotation is the next
increment on top of that.

## The trait: `Provider::mirror_rotated_token`

```rust
fn mirror_rotated_token(&self, blob: &str) -> PResult<()>
```

One method, called exactly once per successful **inactive**-account
rotation (from `credentials::refresh_inactive_if_stale`), right after usagio
writes the refreshed tokens to `state.json`. Its job: push that same blob
back out to wherever the vendor CLI would look for it if the user made this
account active tomorrow. Without this mirror step, `state.json` runs ahead of
the vendor's own on-disk/keychain copy — so the moment the user switches to
that account, the vendor CLI reads its stale copy, tries to use an already-
rotated (and now dead) refresh token, and the user is back at `/login`. This
mirror step is what makes "inactive accounts silently stay fresh while
you're not looking" actually true end-to-end, not just true in `state.json`.

It is deliberately **not** routed through `write_active_account` by default
for providers that override it — mirroring a rotated token must not touch
identity (email, `~/.claude.json`), only the token bytes, because the
account being mirrored is not (necessarily) the active one.

Per-OS behavior is decided by the implementation, not the trait:

- **Claude** overrides it to branch on `Platform::os_display_name()` (a
  runtime call through the `Platform` trait — never `cfg(target_os)`, which
  is reserved for `src/platform/`): macOS writes to the keychain under the
  literal `"Claude Code-credentials"` service name Claude Code itself uses;
  Linux/Windows write the plaintext `~/.claude/.credentials.json`
  (`%USERPROFILE%\.claude\.credentials.json`) file Claude Code reads there
  instead.
- **Codex** overrides it to write `auth.json` unconditionally — Codex has one
  set of credentials per machine with no separate identity file to gate on,
  so the trait's default (which requires a non-`Unsupported`
  `read_active_identity()`) would always fail for it.
- The **trait default** delegates to `write_active_account` using whatever
  `read_active_identity()` currently reports. Providers with no
  identity/switching support inherit `Unsupported` transitively — correct,
  since there's nowhere vendor-native to mirror a rotated token to yet.

## CAS mechanics on macOS

The macOS `SecretStore::set` implementation shells out to `security(1)`
rather than linking Security.framework directly, specifically to sidestep
the "always allow?" SecurityAgent prompt that framework calls trigger for
unsigned, brew-installed binaries.

`[TBD: verify against CAS PR]` — as merged today, `set` calls
`security add-generic-password -U …`, which updates the existing item in
place. The CAS design being built calls for replacing `-U` with an explicit
**delete-then-add**: `security delete-generic-password` followed by a plain
`security add-generic-password` (no `-U`). The stated reason is that `-U`
still occasionally re-triggers the cross-app ACL prompt when a *different*
process (Claude Code itself, mid-rotation) owns the keychain item's ACL —
delete-then-add creates a fresh item under usagio's own ACL every time,
avoiding the prompt path entirely. The full CAS cycle is:

1. **before-read** — read the keychain blob, this is the CAS "compare"
   baseline.
2. **POST** — usagio's own refresh request to the vendor's OAuth endpoint,
   off the baseline's refresh token.
3. **after-read** — read the keychain again, immediately before writing.
   If it no longer matches the before-read baseline, something else rotated
   it while usagio's POST was in flight — abandon the write and adopt the
   drift instead (this is the `_cas_lost` / `_skipped_drift` path).
4. **delete-then-add** — only if the after-read still matches the baseline:
   delete the existing item, then add the new blob as a fresh item.

## What's gone

~1,400 lines of pre-0.5.0 machinery that existed to paper over the
active/inactive race are removed or being removed on the CAS branch, now
that the CAS design closes the race directly instead of working around it:

- `absorb_all_lagging`
- `last_chance_fallback`
- `ensure_fresh_with_fallback`
- fsnotify-based credential-file watching
- the reentrant lock that coordinated all of the above

These are deferred to a v0.5.1 cleanup pass once CAS is proven in
production — `[TBD: verify against CAS PR]` on exact removal scope; as of
this writing `absorb_all_lagging` and `last_chance_fallback` are still
present in `src/credentials.rs` pending that cleanup.

## Adding OS #4

Implement `src/platform/<os>.rs` satisfying the `Platform` trait's five/six
methods (`menu`, `secrets`, `autostart`, `paths`, `os_display_name`,
`secure_permissions`; `file_dialog` has a default). `secrets()` in
particular must implement `SecretStore::{get,set,delete,list}` using
whatever OS-native store is appropriate (Keychain / secret-service /
Credential Manager) — this is the seam every provider's
`mirror_rotated_token` and CAS rotation route through, so an OS backend that
lies about `set`/`get` round-tripping breaks the invariant for every
provider at once.

## Adding vendor #4

Implement `src/providers/<slug>/mod.rs` against the `Provider` trait
(`src/providers/trait_def.rs`), register it in `providers::init()`, and:

- If the vendor CLI has its own OS-native credential storage (keychain,
  credential manager, or an on-disk file it reads outside of usagio's
  control), implement `mirror_rotated_token` explicitly — follow the
  Claude/Codex pattern above rather than relying on the default, since the
  default assumes identity-gated `write_active_account` is the right target
  and that's often wrong for a mirror (it shouldn't touch identity).
- Otherwise, the trait default is fine as-is.
