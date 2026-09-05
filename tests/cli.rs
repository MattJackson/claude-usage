//! Black-box CLI integration tests. Each test runs the compiled `claude-usage`
//! binary as a subprocess with an isolated `HOME` (a fresh tempdir), so it never
//! touches the real `~/.config/claude-usage/state.json`, `~/.claude.json`, the
//! macOS Keychain, or the network. Only argument-validation and offline paths
//! (`--help`, empty-state `list`, `rm`) are exercised.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// A `claude-usage` command pinned to an isolated HOME.
fn bin(home: &TempDir) -> Command {
    let mut c = Command::cargo_bin("claude-usage").expect("binary builds");
    c.env("HOME", home.path());
    c
}

fn state_path(home: &TempDir) -> PathBuf {
    home.path()
        .join(".config")
        .join("claude-usage")
        .join("state.json")
}

fn seed_state(home: &TempDir, json: &str) {
    let p = state_path(home);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, json).unwrap();
}

/// Two fake accounts, `active` null so `rm` behaves cleanly. Field names match
/// the `State`/`Account` serde shape (snake_case). `keychain_blob` is only parsed
/// on capture, never on load, so a dummy value is fine here.
const TWO_ACCOUNTS: &str = r#"{
  "accounts": [
    {"name":"Personal","email":"matthew@pq.io","access_token":"a","refresh_token":"b","expires_at":0,"keychain_blob":"{}","oauth_account":null,"user_id":null},
    {"name":"dev1","email":"dev@getbusbar.com","access_token":"c","refresh_token":"d","expires_at":0,"keychain_blob":"{}","oauth_account":null,"user_id":null}
  ],
  "active": null,
  "autoswap_disabled": false,
  "trigger_pct": null
}"#;

fn account_names_lower(home: &TempDir) -> Vec<String> {
    let s = fs::read_to_string(state_path(home)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    v["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_lowercase())
        .collect()
}

#[test]
fn help_lists_subcommands() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-usage capture"))
        .stdout(predicate::str::contains("switch"))
        .stdout(predicate::str::contains("start"))
        .stdout(predicate::str::contains("continue"))
        .stdout(predicate::str::contains("token"))
        .stdout(predicate::str::contains("watch"))
        .stdout(predicate::str::contains("report"))
        .stdout(predicate::str::contains("install"))
        .stdout(predicate::str::contains("uninstall"))
        .stdout(predicate::str::contains("rm <name>"));
}

#[test]
fn help_subcommand_matches_flag() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .arg("help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "usage & instant account switching",
        ));
}

#[test]
fn unknown_command_exits_two() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .arg("bogus")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown command"));
}

#[test]
fn bare_list_with_no_accounts() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .assert()
        .success()
        .stdout(predicate::str::contains("No accounts yet"));
}

#[test]
fn rm_missing_account_errors() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .args(["rm", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no account named"));
}

#[test]
fn switch_with_no_accounts_errors() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .args(["switch", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no accounts yet"));
}

#[test]
fn capture_requires_a_name() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .arg("capture")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "usage: claude-usage capture <name>",
        ));
}

#[test]
fn rm_removes_seeded_account() {
    let home = TempDir::new().unwrap();
    seed_state(&home, TWO_ACCOUNTS);
    bin(&home)
        .args(["rm", "dev1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed 'dev1'"));
    let names = account_names_lower(&home);
    assert!(!names.contains(&"dev1".to_string()), "dev1 should be gone");
    assert!(
        names.contains(&"personal".to_string()),
        "Personal should remain"
    );
}

#[test]
fn rm_is_case_insensitive() {
    let home = TempDir::new().unwrap();
    seed_state(&home, TWO_ACCOUNTS);
    bin(&home)
        .args(["rm", "PERSONAL"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed 'PERSONAL'"));
    let names = account_names_lower(&home);
    assert!(
        !names.contains(&"personal".to_string()),
        "Personal should be removed despite different casing"
    );
    assert!(names.contains(&"dev1".to_string()), "dev1 should remain");
}
