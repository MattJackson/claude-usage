//! `usagio context [--provider <slug>] [--project <path>]` subcommand.
//!
//! When no `--provider` is given, iterates the built-in known-CLI list.
//! Providers not registered in the trait registry are silently skipped.

use super::{build_ledger, render_terminal};
use std::path::PathBuf;

pub fn run(provider: Option<String>, project: Option<PathBuf>) -> anyhow::Result<()> {
    let providers = match provider {
        Some(p) => vec![p],
        None => vec![
            "claude".to_string(),
            "codex".to_string(),
            "opencode".to_string(),
        ],
    };
    // L8 (round-1 codeaudit): render one section per provider unconditionally
    // so the output order matches the iteration order (deterministic) and a
    // provider with no items still surfaces its heading. Previously an empty
    // ledger was suppressed until at least one other provider had already
    // printed, so identical inputs could yield different presence-of-headings
    // depending on which provider iterated first.
    let mut first = true;
    let mut printed_any = false;
    for prov in providers {
        match build_ledger(&prov, project.as_deref()) {
            Ok(ledger) => {
                if !first {
                    println!();
                }
                print!("{}", render_terminal(&ledger));
                first = false;
                printed_any = true;
            }
            Err(super::LedgerError::UnknownProvider(_)) => {
                eprintln!("usagio context: unknown provider '{}'", prov);
            }
            Err(err) => {
                eprintln!("usagio context: {} failed: {}", prov, err);
            }
        }
    }
    if !printed_any {
        println!("Context Ledger — no context items discovered for any provider.");
    }
    Ok(())
}
