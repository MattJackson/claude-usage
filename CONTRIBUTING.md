# Contributing to claude-usage

Thanks for your interest! `claude-usage` is a macOS-only Rust menu-bar app + CLI
for tracking and switching between Claude Max accounts. Contributions are welcome.

## Ground rules

- **Never log tokens.** OAuth access/refresh tokens must never be written to logs,
  stdout, error messages, or crash output. Redact anything token-shaped.
- **Keep token files owner-only.** Anything that stores credentials
  (`~/.config/claude-usage/state.json`) must be created and kept `chmod 600`; the
  macOS Keychain item is the other trusted store. Never widen these permissions.
- **First-party endpoints only.** Tokens go to Anthropic/Claude's own endpoints and
  nowhere else. Don't add a code path that sends credentials to any other host.
- **Don't break the auto-swap safety margin.** The auto-swap logic exists to keep a
  session below the hard 100% wall with hysteresis (threshold, cooldown, and
  no-bounce-back). Changes here must preserve that safety margin — don't remove the
  headroom checks or let it thrash.

## Workflow

- Branch from and open PRs against **`dev`**. CI on macOS runs `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --all-features` on every push/PR.
- **`qa`** is the release-candidate branch and **`main`** is the release branch;
  changes flow `dev` → `qa` → `main`.
- Keep the gate green locally before pushing:

  ```sh
  cargo fmt --all
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --all-features
  ```

## License

By contributing you agree your contributions are licensed under the MIT License.
