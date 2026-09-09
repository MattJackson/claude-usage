use super::*;
use base64::Engine;
use std::io::{Read, Write};
use std::net::TcpListener;

fn make_id_token(email: &str, exp: i64) -> String {
    let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::json!({ "email": email, "exp": exp })
            .to_string()
            .as_bytes(),
    );
    format!("{hdr}.{payload}.sig")
}

fn make_auth_json(
    email: &str,
    exp: i64,
    access: &str,
    refresh: &str,
    last_refresh: Option<&str>,
) -> String {
    let id_token = make_id_token(email, exp);
    let mut v = serde_json::json!({
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": refresh,
        }
    });
    if let Some(lr) = last_refresh {
        v.as_object_mut().unwrap().insert(
            "last_refresh".into(),
            serde_json::Value::String(lr.to_string()),
        );
    }
    v.to_string()
}

/// Run `f` with `$CODEX_HOME` set to `codex_home` AND
/// `$CODEX_REFRESH_TOKEN_URL_OVERRIDE` set to `token_url`, serialised on the
/// crate-wide `ENV_LOCK` exactly once. `scoped_env_var` isn't reentrant (see
/// its doc comment), so a second `scoped_env_var` call inside its own
/// closure would deadlock; the crate's ENV_LOCK is already held for the
/// whole closure below, so mutating the second var directly here is safe —
/// no other test can observe or race it. Mirrors the documented escape
/// hatch in `clippy.toml` for exactly this situation.
fn with_codex_env<R>(codex_home: &str, token_url: &str, f: impl FnOnce() -> R) -> R {
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(codex_home), || {
        let prev = std::env::var_os("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
        #[allow(clippy::disallowed_methods)]
        std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", token_url);
        let r = f();
        #[allow(clippy::disallowed_methods)]
        match prev {
            Some(v) => std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", v),
            None => std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
        }
        r
    })
}

/// Minimal single-shot HTTP server: accepts one connection, drains the
/// request, and writes back `body` with `status` as a JSON response. Good
/// enough to stand in for Codex's `/oauth/token` endpoint in tests — we
/// never need more than one refresh round trip per test.
///
/// `on_request` runs AFTER the request is fully read but BEFORE the response
/// is written, in the server's background thread — used by the "lost the
/// CAS race" test to simulate a concurrent `codex` CLI rotating `auth.json`
/// while our refresh POST is in flight.
fn spawn_mock_token_server(
    status: u16,
    body: String,
    on_request: impl FnOnce() + Send + 'static,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("local_addr");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = match listener.accept() {
            Ok(x) => x,
            Err(_) => return,
        };
        // Read headers.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_double_crlf(&buf) {
                break Some(pos);
            }
            if buf.len() > 64 * 1024 {
                break None;
            }
        };
        if let Some(pos) = header_end {
            let header_str = String::from_utf8_lossy(&buf[..pos]);
            let content_length: usize = header_str
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().to_string())
                })
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let already_have = buf.len() - (pos + 4);
            let mut remaining = content_length.saturating_sub(already_have);
            while remaining > 0 {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                remaining = remaining.saturating_sub(n);
            }
        }

        on_request();

        let status_line = match status {
            200 => "200 OK",
            400 => "400 Bad Request",
            401 => "401 Unauthorized",
            429 => "429 Too Many Requests",
            _ => "500 Internal Server Error",
        };
        let resp = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
    });
    (format!("http://{addr}/oauth/token"), handle)
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// --- refresh_token_grant -----------------------------------------------------

#[test]
fn refresh_token_grant_rejects_empty_token_without_network() {
    let err = refresh_token_grant("").unwrap_err();
    assert!(matches!(err, RefreshError::InvalidGrant));
}

#[test]
fn refresh_token_grant_maps_400_to_invalid_grant() {
    let (url, handle) = spawn_mock_token_server(
        400,
        r#"{"error":{"code":"refresh_token_reused"}}"#.into(),
        || {},
    );
    crate::env_lock::scoped_env_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", Some(&url), || {
        let err = refresh_token_grant("stale-rt").unwrap_err();
        assert!(matches!(err, RefreshError::InvalidGrant));
    });
    let _ = handle.join();
}

#[test]
fn refresh_token_grant_maps_429_to_rate_limited() {
    let (url, handle) = spawn_mock_token_server(429, "{}".into(), || {});
    crate::env_lock::scoped_env_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", Some(&url), || {
        let err = refresh_token_grant("rt").unwrap_err();
        assert!(matches!(err, RefreshError::RateLimited));
    });
    let _ = handle.join();
}

#[test]
fn refresh_token_grant_parses_successful_response() {
    let new_id_token = make_id_token("u@e.com", chrono::Utc::now().timestamp() + 3600);
    let body = serde_json::json!({
        "access_token": "new-at",
        "refresh_token": "new-rt",
        "id_token": new_id_token,
    })
    .to_string();
    let (url, handle) = spawn_mock_token_server(200, body, || {});
    crate::env_lock::scoped_env_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", Some(&url), || {
        let grant = refresh_token_grant("old-rt").unwrap();
        assert_eq!(grant.access_token, "new-at");
        assert_eq!(grant.refresh_token, "new-rt");
        assert!(grant.expires_in_secs > 0);
    });
    let _ = handle.join();
}

#[test]
fn refresh_token_grant_keeps_refresh_token_if_server_omits_it() {
    let body = serde_json::json!({ "access_token": "new-at" }).to_string();
    let (url, handle) = spawn_mock_token_server(200, body, || {});
    crate::env_lock::scoped_env_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", Some(&url), || {
        let grant = refresh_token_grant("old-rt").unwrap();
        assert_eq!(grant.refresh_token, "old-rt");
    });
    let _ = handle.join();
}

// --- active_refresh_cas -------------------------------------------------------
//
// The active-account contract is now NEVER-POST: usagio is a pure follower of
// the slot the Codex CLI owns (`auth.json`). Codex's refresh tokens are
// single-use, so a usagio POST would rotate the family server-side and force a
// `codex login` (see `active_refresh_cas`'s doc, and the Claude analog). Every
// test below therefore points `CODEX_REFRESH_TOKEN_URL_OVERRIDE` at a listener
// that counts connections and asserts ZERO were made — if a regression ever
// reintroduced a `/token` POST for the active account, the counter would catch
// it — and asserts the function adopts whatever the slot holds.

/// A TCP listener that counts inbound connections. Used to prove the
/// active-account refresh path makes NO HTTP call: we point
/// `CODEX_REFRESH_TOKEN_URL_OVERRIDE` at it and assert the counter stays 0.
/// The accept loop leaks a thread for the test's lifetime (same pattern as
/// `spawn_mock_token_server`); the atomic is checked only after the
/// synchronous `active_refresh_cas` call has already returned, so any POST it
/// might have made would have completed and been counted by then.
fn spawn_connection_counter() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind connection counter");
    let addr = listener.local_addr().expect("local_addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() {
                hits_thread.fetch_add(1, Ordering::SeqCst);
            } else {
                break;
            }
        }
    });
    (format!("http://{addr}/oauth/token"), hits)
}

#[test]
fn codex_active_refresh_cas_adopts_slot_without_posting() {
    // active_refresh_cas reaches logging::log → store::config_dir(), which
    // panics in tests without a HOME_OVERRIDE. Wrap in ScopedConfigDir so
    // the tripwire doesn't fire.
    let _g = crate::store::ScopedConfigDir::new();
    // Even with an EXPIRED access token and no last-known blob, the active
    // path must NOT POST /token — it adopts the slot exactly as-is. (Under the
    // old bug this expired token would have driven a token mint.)
    let dir = tempfile::tempdir().unwrap();
    let expired = chrono::Utc::now().timestamp() - 10;
    let initial = make_auth_json("u@e.com", expired, "old-at", "old-rt", None);
    std::fs::write(dir.path().join("auth.json"), &initial).unwrap();

    let (url, hits) = spawn_connection_counter();
    let outcome = with_codex_env(dir.path().to_str().unwrap(), &url, || {
        active_refresh_cas(None).unwrap()
    });

    match outcome {
        CasOutcome::Adopted(blob) => assert_eq!(blob, initial, "must adopt the slot verbatim"),
        other => panic!("expected Adopted(slot), got {other:?}"),
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "active-account refresh must make ZERO HTTP calls"
    );
    // The slot is left byte-for-byte untouched — no write, no rotation.
    let on_disk = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
    assert_eq!(on_disk, initial, "adopt must never rewrite auth.json");
}

#[test]
fn codex_active_refresh_cas_lost_adopts_cli_rotation() {
    let _g = crate::store::ScopedConfigDir::new();
    // The Codex CLI rotated auth.json since usagio last observed it
    // (`last_known` = the old blob). usagio must adopt the CLI's current blob
    // and never POST — the CLI already holds a valid, freshly-minted grant.
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let expired = chrono::Utc::now().timestamp() - 10;
    let last_known = make_auth_json("u@e.com", expired, "old-at", "old-rt", None);

    let cli_rotated = make_auth_json(
        "u@e.com",
        chrono::Utc::now().timestamp() + 7200,
        "cli-rotated-at",
        "cli-rotated-rt",
        None,
    );
    // Disk already holds what the CLI rotated to.
    std::fs::write(&auth_path, &cli_rotated).unwrap();

    let (url, hits) = spawn_connection_counter();
    let outcome = with_codex_env(dir.path().to_str().unwrap(), &url, || {
        active_refresh_cas(Some(&last_known)).unwrap()
    });

    match outcome {
        CasOutcome::Adopted(blob) => assert_eq!(blob, cli_rotated),
        other => panic!("expected Adopted(cli_rotated), got {other:?}"),
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "adopting a CLI rotation must make ZERO HTTP calls"
    );
    // The file on disk must still be exactly what the CLI wrote — usagio must
    // never write over it.
    let on_disk = std::fs::read_to_string(&auth_path).unwrap();
    assert_eq!(on_disk, cli_rotated);
}

#[test]
fn codex_active_refresh_cas_fresh_when_slot_matches_last_known() {
    // When the slot on disk still equals the caller's last-known blob, the CLI
    // hasn't rotated anything: return Fresh, make no HTTP call, and never
    // rewrite the file.
    let _g = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let blob = make_auth_json(
        "u@e.com",
        chrono::Utc::now().timestamp() + 3600,
        "at",
        "rt",
        Some(&chrono::Utc::now().to_rfc3339()),
    );
    std::fs::write(&auth_path, &blob).unwrap();
    let mtime_before = std::fs::metadata(&auth_path).unwrap().modified().unwrap();

    let (url, hits) = spawn_connection_counter();
    let outcome = with_codex_env(dir.path().to_str().unwrap(), &url, || {
        active_refresh_cas(Some(&blob)).unwrap()
    });
    assert_eq!(outcome, CasOutcome::Fresh);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a Fresh outcome must make ZERO HTTP calls"
    );

    let on_disk = std::fs::read_to_string(&auth_path).unwrap();
    assert_eq!(
        on_disk, blob,
        "a Fresh outcome must never rewrite auth.json"
    );
    let mtime_after = std::fs::metadata(&auth_path).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after);
}

#[test]
fn codex_active_refresh_cas_adopts_when_last_known_blob_already_stale() {
    let _g = crate::store::ScopedConfigDir::new();
    // The caller's cached reference doesn't match what's on disk — the CLI
    // rotated the file behind our back. Adopt it immediately, no network call.
    let dir = tempfile::tempdir().unwrap();
    let current = make_auth_json(
        "u@e.com",
        chrono::Utc::now().timestamp() + 3600,
        "at",
        "rt",
        None,
    );
    std::fs::write(dir.path().join("auth.json"), &current).unwrap();

    let (url, hits) = spawn_connection_counter();
    let outcome = with_codex_env(dir.path().to_str().unwrap(), &url, || {
        active_refresh_cas(Some("stale-cached-blob")).unwrap()
    });
    match outcome {
        CasOutcome::Adopted(blob) => assert_eq!(blob, current),
        other => panic!("expected Adopted, got {other:?}"),
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "stale-cache adopt must make ZERO HTTP calls"
    );
}

#[test]
fn codex_active_refresh_cas_adopts_slot_even_without_refresh_token() {
    // A follower never needs a refresh token: even a slot with no
    // `refresh_token` at all is adopted verbatim (no error, no POST) — the
    // active blob is by definition whatever the CLI is using.
    let _g = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let blob = serde_json::json!({
        "tokens": {
            "id_token": make_id_token("u@e.com", chrono::Utc::now().timestamp() - 10),
            "access_token": "at",
        }
    })
    .to_string();
    std::fs::write(dir.path().join("auth.json"), &blob).unwrap();

    let (url, hits) = spawn_connection_counter();
    let outcome = with_codex_env(dir.path().to_str().unwrap(), &url, || {
        active_refresh_cas(None).unwrap()
    });
    match outcome {
        CasOutcome::Adopted(adopted) => assert_eq!(adopted, blob),
        other => panic!("expected Adopted even without a refresh_token, got {other:?}"),
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "adopt of a refresh-token-less slot must make ZERO HTTP calls"
    );
}

/// robustness-01: a server that accepts the TCP connection but never writes a
/// response must not hang the caller forever. Before the shared
/// `http_agent()` (with `.timeout_read()` set) existed, the default
/// `ureq::Agent` only bounded the TCP *connect*, so this exact scenario
/// (connection accepted, response withheld) would block indefinitely.
#[test]
fn refresh_token_grant_read_timeout_fails_fast_not_forever() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling server");
    let addr = listener.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        // Accept the connection and hold it open well past the client's read
        // timeout without ever writing a response byte — the connection is
        // live, the server just never answers.
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(std::time::Duration::from_secs(30));
            drop(stream);
        }
    });
    let url = format!("http://{addr}/oauth/token");
    crate::env_lock::scoped_env_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", Some(&url), || {
        let start = std::time::Instant::now();
        let err = refresh_token_grant("rt").expect_err("stalled server must not succeed");
        let elapsed = start.elapsed();
        assert!(
            matches!(err, RefreshError::Transient(_)),
            "expected a transient (timeout) error, got {err:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "expected the read timeout to fire well under 20s, took {elapsed:?}"
        );
    });
}
