# Fixtures

Each file here is a template for a `state.json` usagio will load as-is —
real UI, mocked data. `render_fixture.py` (in the parent dir) resolves any
`NOW+<duration>` tokens to an absolute timestamp right before a capture
script copies the result into `~/.config/usagio/state.json` (or
`%APPDATA%\usagio\state.json` on Windows). See the top of
`render_fixture.py` for why the countdown-driven fixtures can't use a
static date.

All emails, tokens (`demo-*`), and UUIDs are obviously fake placeholders —
none of these files are secrets and all are safe to commit.

| Fixture | Accounts | Demonstrates | Variant(s) captured from it |
|---|---|---|---|
| `healthy.json` | 2 Claude (`demo1@example.com` 47%/89%, `dev@example.com` 2%/98%) + 3 Codex (`alice`/`bob`/`carol`@codex.example, 33-78%) | Normal day-to-day state — nothing locked, no re-login needed. The "everything working" shot. | `<os>-tray.png`, `<os>-menu-healthy.png`, `<os>-settings.png` |
| `weekly-locked.json` | Same account set as `healthy.json`, but `demo1@example.com`'s weekly window is at 100% with `weekly_reset: NOW+2d14h` | The weekly-locked countdown display: usagio's `countdown.rs` treats a window as locked once it's ≥99.5% used with a still-future reset, and swaps that account's row from `"S% / W%"` to a red `"2d 14h"` countdown. | `<os>-menu-locked.png` |
| `session-locked.json` | Same base accounts, but `demo1@example.com`'s session window is at 100% with `session_reset: NOW+3h15m` (weekly is a healthy 47%, well under the lock threshold) | Session-locked-but-weekly-has-room: the row shows a red `"3h 15m"` countdown for the session window. **Note:** the real UI does not combine this with the weekly percentage on the same row (see "Known discrepancy" below) — the weekly 47% is real data on the account, just not rendered inline with the countdown. | `<os>-menu-session-locked.png` |
| `mixed.json` | 4 accounts across both providers: `demo1@example.com` (Claude, active + healthy), `dev@example.com` (Claude, session-locked, `NOW+45m`), `alice@codex.example` (Codex, weekly-locked, `NOW+6h`), `bob@codex.example` (Codex, `needs_relogin: true`) | One menu showing every state at once: healthy, session-locked, weekly-locked, and a stale-refresh-token account. **Note:** `needs_relogin` has no dedicated icon/flag in the current menu row rendering (see "Known discrepancy" below) — `bob`'s row looks like a normal usage row; the field is still set so this fixture stays accurate to what a real "needs re-login" account's `state.json` actually contains, and is ready to pick up a visual treatment if one gets added later. | `<os>-menu-mixed.png` |

`<os>-tray.png` and `<os>-settings.png` both reuse `healthy.json` rather
than getting a dedicated fixture — neither the tray icon/title nor the
Settings submenu's structure depends on which accounts are loaded, so a
second near-identical fixture would just be duplication.

## Known discrepancies vs. the original screenshot-variant spec

While building these fixtures we read the actual rendering code
(`src/menubar.rs`, `src/countdown.rs`) rather than assume what the menu
looks like, since the whole point of this pipeline is *real UI*, not a
plausible-looking fake. Two things came out differently from how the
variants were first described, and we kept the fixtures honest to the real
app instead of the original description:

- **No combined "`3h 15m` / `47%`" trailing text exists.** When a window is
  locked, `main_row` in `menubar.rs` replaces the percentage pair with
  *just* the countdown string, colored red — never combined with the other
  window's percentage. `session-locked.json` is built to trigger the real
  countdown display (`"3h 15m"`), not a hybrid string the app doesn't
  render.
- **`needs_relogin` has no menu-row icon/flag today.** It's real account
  state (gates auto-swap eligibility, drives the re-login flow elsewhere)
  but is invisible in the current row rendering — `AcctView` doesn't even
  carry the field through to the menu. `mixed.json` still sets
  `needs_relogin: true` on `bob@codex.example` for data accuracy, but
  `<os>-menu-mixed.png` won't visually distinguish that row from a normal
  one until/unless a UI treatment for it is added.
- **The Settings submenu isn't "Refresh at top, then a rule, then
  alphabetical."** The real structure (`build_menu` in `menubar.rs`) is
  fixed-order: `Notifications ▸`, then `Auto-swap threshold ▸`, then
  `Advanced ▸` (which itself contains `Backups ▸` followed by "Refresh
  usage now" — last, not first). `<os>-settings.png` captures that real
  structure as-is.

## Website consumption

- **Hero rotation on `/`** uses `<os>-menu-healthy.png` — the
  "everything working" shot.
- **`/download`'s per-OS section** uses `<os>-menu-mixed.png` — "here's
  what it looks like in real use," showing every account state at once.
- **Features grid** — individual variations illustrate individual
  features: `<os>-menu-locked.png` for the weekly-lock countdown feature,
  `<os>-menu-session-locked.png` for session-vs-weekly window tracking,
  `<os>-settings.png` for the notifications/auto-swap/backups settings
  surface.

Wiring these into `web/src/pages/*.astro` is not part of this automation
(`web/**` is out of scope for `packaging/screenshots/` and its workflow) —
this table is the contract for whoever does that wiring. Note that
`web/public/screenshots/menu-healthy.svg`, `menu-mixed.svg`, and
`menu-locked.svg` are the hand-drawn mockups these real captures are meant
to eventually replace on `index.astro`.
