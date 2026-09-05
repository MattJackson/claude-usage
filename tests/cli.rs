//! Black-box CLI integration tests. Each test runs the compiled `claude-usage`
//! binary as a subprocess with an isolated `HOME` (a fresh tempdir), so it never
//! touches the real `~/.config/claude-usage/state.json`, `~/.claude.json`, or the
//! network. (Keychain access isn't HOME-scoped, so `capture` is not exercised
//! here.) Accounts are keyed by email.

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

/// Two fake accounts in the email-keyed shape. `keychain_blob` is only parsed on
/// capture, never on load, so a dummy value is fine here.
const TWO_ACCOUNTS: &str = r#"{
  "accounts": [
    {"email":"matthew@pq.io","access_token":"a","refresh_token":"b","expires_at":0,"keychain_blob":"{}","oauth_account":null,"user_id":null,"cached_usage":null},
    {"email":"dev@getbusbar.com","access_token":"c","refresh_token":"d","expires_at":0,"keychain_blob":"{}","oauth_account":null,"user_id":null,"cached_usage":null}
  ],
  "active": null,
  "autoswap_disabled": false,
  "trigger_pct": null
}"#;

/// The legacy name-keyed shape, to prove migration on load.
const OLD_SHAPE: &str = r#"{
  "accounts": [
    {"name":"dev1","email":"dev@getbusbar.com","access_token":"c","refresh_token":"d","expires_at":0,"keychain_blob":"{}"},
    {"name":"Personal","email":"matthew@pq.io","access_token":"a","refresh_token":"b","expires_at":0,"keychain_blob":"{}"}
  ],
  "active": "Personal"
}"#;

fn account_emails_lower(home: &TempDir) -> Vec<String> {
    let s = fs::read_to_string(state_path(home)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    v["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["email"].as_str().unwrap().to_lowercase())
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
        .stdout(predicate::str::contains("rm <email>"));
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
fn version_flag() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-usage "));
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
    seed_state(&home, TWO_ACCOUNTS);
    bin(&home)
        .args(["rm", "nobody@example.com"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no account matches"));
}

#[test]
fn switch_with_no_accounts_errors() {
    let home = TempDir::new().unwrap();
    bin(&home)
        .args(["switch", "nope@example.com"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no accounts yet"));
}

#[test]
fn rm_removes_seeded_account_by_prefix() {
    let home = TempDir::new().unwrap();
    seed_state(&home, TWO_ACCOUNTS);
    // Unique prefix "dev" resolves to dev@getbusbar.com.
    bin(&home)
        .args(["rm", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed dev@getbusbar.com"));
    let emails = account_emails_lower(&home);
    assert!(!emails.contains(&"dev@getbusbar.com".to_string()));
    assert!(emails.contains(&"matthew@pq.io".to_string()));
}

#[test]
fn rm_is_case_insensitive() {
    let home = TempDir::new().unwrap();
    seed_state(&home, TWO_ACCOUNTS);
    bin(&home)
        .args(["rm", "MATTHEW@PQ.IO"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed matthew@pq.io"));
    let emails = account_emails_lower(&home);
    assert!(!emails.contains(&"matthew@pq.io".to_string()));
    assert!(emails.contains(&"dev@getbusbar.com".to_string()));
}

#[test]
fn rm_ambiguous_prefix_errors() {
    let home = TempDir::new().unwrap();
    seed_state(
        &home,
        r#"{"accounts":[
            {"email":"dev1@e.com","access_token":"a","refresh_token":"b","expires_at":0,"keychain_blob":"{}"},
            {"email":"dev2@e.com","access_token":"c","refresh_token":"d","expires_at":0,"keychain_blob":"{}"}
        ],"active":null}"#,
    );
    bin(&home)
        .args(["rm", "dev"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous"));
}

#[test]
fn migrates_old_name_keyed_state_on_load() {
    let home = TempDir::new().unwrap();
    seed_state(&home, OLD_SHAPE);
    // `list` reads (and migrates) the old shape; accounts show by email.
    bin(&home)
        .assert()
        .success()
        .stdout(predicate::str::contains("dev@getbusbar.com"))
        .stdout(predicate::str::contains("matthew@pq.io"));
}
