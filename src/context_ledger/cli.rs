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
    let mut printed_any = false;
    for prov in providers {
        match build_ledger(&prov, project.as_deref()) {
            Ok(ledger) => {
                if !ledger.items.is_empty() || printed_any {
                    if printed_any {
                        println!();
                    }
                    print!("{}", render_terminal(&ledger));
                    printed_any = true;
                }
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
