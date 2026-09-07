//! Unit tests for the Context Ledger.
//!
//! Uses `tempfile` to sandbox HOME-relative paths. Some tests set HOME to a
//! temp dir to isolate ~/.claude discovery.

use super::*;
use std::fs;
use tempfile::tempdir;

/// Every test that mutates `$HOME` here is serialised against the *rest of
/// the crate* via `crate::env_lock::scoped_env_var`. Local mutexes weren't
/// enough — the never-re-login incident was caused by a different in-crate
/// test module setting `$HOME` outside this local lock, racing us mid-scope.
fn with_home<F: FnOnce(&std::path::Path)>(f: F) {
    let td = tempdir().unwrap();
    let path = td.path().to_path_buf();
    let path_str = path.to_str().expect("tempdir path is valid UTF-8");
    crate::env_lock::scoped_env_var("HOME", Some(path_str), || {
        f(&path);
    });
}

#[test]
fn empty_state_yields_empty_ledger_claude() {
    with_home(|_| {
        let l = build_ledger("claude", None).unwrap();
        assert!(l.items.is_empty(), "expected empty, got {:?}", l.items);
        assert_eq!(l.total_tokens, 0);
        assert_eq!(l.provider, "claude");
    });
}

#[test]
fn empty_state_yields_empty_ledger_codex() {
    with_home(|_| {
        let l = build_ledger("codex", None).unwrap();
        assert!(l.items.is_empty());
    });
}

#[test]
fn empty_state_yields_empty_ledger_opencode() {
    with_home(|_| {
        let l = build_ledger("opencode", None).unwrap();
        assert!(l.items.is_empty());
    });
}

#[test]
fn unknown_provider_errors() {
    with_home(|_| {
        let e = build_ledger("nope", None).unwrap_err();
        assert!(matches!(e, LedgerError::UnknownProvider(_)));
    });
}

#[test]
fn claude_global_md_counted() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        let content = "hello ".repeat(100); // ~600 chars → ~165 tokens by approx
        fs::write(dir.join("CLAUDE.md"), &content).unwrap();
        let l = build_ledger("claude", None).unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].kind, ItemKind::GlobalInstructions);
        assert!(l.items[0].token_count > 50);
        assert!(l.items[0].token_count < 300);
    });
}

#[test]
fn claude_project_md_counted() {
    with_home(|_| {
        let proj = tempdir().unwrap();
        fs::write(proj.path().join("CLAUDE.md"), "project instructions").unwrap();
        let l = build_ledger("claude", Some(proj.path())).unwrap();
        assert!(l.items.iter().any(|i| i.kind == ItemKind::ProjectInstructions));
    });
}

#[test]
fn stale_when_file_touched_after_capture() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("CLAUDE.md");
        fs::write(&p, "v1").unwrap();
        let l = build_ledger("claude", None).unwrap();
        // Sleep briefly so mtime is strictly greater than captured_at.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&p, "v2").unwrap();
        assert!(is_stale(&l), "ledger should be stale after file mtime bump");
    });
}

#[test]
fn mcp_missing_binary_skipped_not_fatal() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        // settings.json with a non-existent MCP binary — collect() should NOT
        // return an error; the failed server is just omitted from items.
        let settings = serde_json::json!({
            "mcpServers": {
                "does-not-exist": {
                    "command": "/definitely/not/a/binary",
                    "args": []
                }
            }
        });
        fs::write(
            dir.join("settings.json"),
            serde_json::to_string(&settings).unwrap(),
        )
        .unwrap();
        let l = build_ledger("claude", None).unwrap();
        assert!(l.items.iter().all(|i| i.kind != ItemKind::McpTools));
    });
}

#[test]
fn tokenize_approx_within_range() {
    // ~1000 chars → ~275 tokens by approx (chars*11/40)
    let text = "a".repeat(1000);
    let n = tokenize::count_tokens(&text, tokenize::TokenizerHint::Anthropic);
    assert!((250..=300).contains(&n), "got {}", n);
}

#[test]
fn opencode_prompt_counted() {
    with_home(|home| {
        let dir = home.join(".config").join("opencode");
        fs::create_dir_all(&dir).unwrap();
        let cfg = r#"{
  // A comment inside JSONC
  "prompt": "You are a helpful assistant.",
  "rules": ["be terse", "use plain text"]
}"#;
        fs::write(dir.join("opencode.jsonc"), cfg).unwrap();
        let l = build_ledger("opencode", None).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"opencode prompt"), "names: {:?}", names);
        assert!(names.contains(&"opencode rules"));
    });
}

#[test]
fn total_tokens_sums_items() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("CLAUDE.md"), "hello world").unwrap();
        let l = build_ledger("claude", None).unwrap();
        let sum: usize = l.items.iter().map(|i| i.token_count).sum();
        assert_eq!(l.total_tokens, sum);
    });
}
