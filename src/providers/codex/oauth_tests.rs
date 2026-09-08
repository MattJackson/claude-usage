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

// --- grant_needs_refresh ----------------------------------------------------

#[test]
fn grant_needs_refresh_false_for_far_future_exp_and_recent_last_refresh() {
    let now = chrono::Utc::now();
    let blob = make_auth_json(
        "u@e.com",
        now.timestamp() + 3600,
        "at",
        "rt",
        Some(&now.to_rfc3339()),
    );
    assert!(!grant_needs_refresh(&blob, 900, 8 * 86400));
}

#[test]
fn grant_needs_refresh_true_when_access_token_near_expiry() {
    let now = chrono::Utc::now();
    let blob = make_auth_json(
        "u@e.com",
        now.timestamp() + 60,
        "at",
        "rt",
        Some(&now.to_rfc3339()),
    );
    assert!(grant_needs_refresh(&blob, 900, 8 * 86400));
}

#[test]
fn grant_needs_refresh_true_when_last_refresh_older_than_session_ttl() {
    // Access token itself still has a year of life, but last_refresh is 9
    // days old — the vendor's own ~8-day session cadence should fire.
    let now = chrono::Utc::now();
    let stale_last_refresh = now - chrono::Duration::days(9);
    let blob = make_auth_json(
        "u@e.com",
        now.timestamp() + 365 * 86400,
        "at",
        "rt",
        Some(&stale_last_refresh.to_rfc3339()),
    );
    assert!(grant_needs_refresh(&blob, 900, 8 * 86400));
}

#[test]
fn grant_needs_refresh_ignores_missing_last_refresh() {
    // No last_refresh at all (never refreshed since login) — only the skew
    // check applies, matching the vendor CLI's own should_refresh_proactively.
    let now = chrono::Utc::now();
    let blob = make_auth_json("u@e.com", now.timestamp() + 365 * 86400, "at", "rt", None);
    assert!(!grant_needs_refresh(&blob, 900, 8 * 86400));
}

#[test]
fn grant_needs_refresh_false_for_unparseable_blob() {
    assert!(!grant_needs_refresh("not json", 900, 8 * 86400));
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

#[test]
fn codex_active_refresh_cas_won_writes_auth_json_and_state() {
    // active_refresh_cas reaches logging::log → store::config_dir(), which
    // panics in tests without a HOME_OVERRIDE. Wrap in ScopedConfigDir so
    // the tripwire doesn't fire.
    let _g = crate::store::ScopedConfigDir::new();
    // "state" for Codex today IS auth.json — there is no separate
    // state.json account slot yet (see codex::mod's absorb_credential doc).
    // So "writes ... and state" here means: the CAS write lands, and it's
    // the one and only place Codex's active-login truth lives.
    let dir = tempfile::tempdir().unwrap();
    let expired = chrono::Utc::now().timestamp() - 10;
    let initial = make_auth_json("u@e.com", expired, "old-at", "old-rt", None);
    std::fs::write(dir.path().join("auth.json"), &initial).unwrap();

    let new_id_token = make_id_token("u@e.com", chrono::Utc::now().timestamp() + 3600);
    let body = serde_json::json!({
        "access_token": "new-at",
        "refresh_token": "new-rt",
        "id_token": new_id_token,
    })
    .to_string();
    let (url, handle) = spawn_mock_token_server(200, body, || {});

    with_codex_env(dir.path().to_str().unwrap(), &url, || {
        let outcome = active_refresh_cas(900, 8 * 86400, None).unwrap();
        assert_eq!(outcome, CasOutcome::Refreshed);
    });
    let _ = handle.join();

    let on_disk = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
    assert_eq!(
        v["tokens"]["access_token"].as_str(),
        Some("new-at"),
        "CAS win must persist the refreshed access_token"
    );
    assert_eq!(v["tokens"]["refresh_token"].as_str(), Some("new-rt"));
    assert!(v["last_refresh"].as_str().is_some());
}

#[test]
fn codex_active_refresh_cas_lost_adopts_cli_rotation() {
    let _g = crate::store::ScopedConfigDir::new();
    // Simulate a `codex` CLI process rotating auth.json WHILE our refresh
    // POST is in flight: the mock server rewrites the file from its
    // background thread before answering our request. active_refresh_cas
    // must detect the mismatch on its post-refresh re-read, discard its own
    // (now úseless) grant, and report the CLI's blob instead of clobbering it.
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let expired = chrono::Utc::now().timestamp() - 10;
    let initial = make_auth_json("u@e.com", expired, "old-at", "old-rt", None);
    std::fs::write(&auth_path, &initial).unwrap();

    let cli_rotated = make_auth_json(
        "u@e.com",
        chrono::Utc::now().timestamp() + 7200,
        "cli-rotated-at",
        "cli-rotated-rt",
        None,
    );
    let auth_path_for_server = auth_path.clone();
    let cli_rotated_for_server = cli_rotated.clone();
    let body =
        serde_json::json!({ "access_token": "our-at", "refresh_token": "our-rt" }).to_string();
    let (url, handle) = spawn_mock_token_server(200, body, move || {
        std::fs::write(&auth_path_for_server, &cli_rotated_for_server).unwrap();
    });

    let outcome = with_codex_env(dir.path().to_str().unwrap(), &url, || {
        active_refresh_cas(900, 8 * 86400, None).unwrap()
    });
    let _ = handle.join();

    match outcome {
        CasOutcome::Adopted(blob) => assert_eq!(blob, cli_rotated),
        other => panic!("expected Adopted(cli_rotated), got {other:?}"),
    }
    // The file on disk must still be exactly what the "CLI" wrote — our
    // stale grant must never have been written over it.
    let on_disk = std::fs::read_to_string(&auth_path).unwrap();
    assert_eq!(on_disk, cli_rotated);
}

#[test]
fn codex_inactive_refresh_atomic_no_toctou() {
    // Named for parity with the Claude-side inactive-refresh atomicity test;
    // Codex has no separate inactive-account store yet (single auth.json is
    // the only tracked login), so the atomicity property under test here is:
    // when the token is already fresh, active_refresh_cas performs NO
    // filesystem write at all (not even a no-op rewrite) — there is no
    // window in which a concurrent reader could observe a torn or
    // needlessly-rewritten file for an account that didn't need refreshing.
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let fresh = make_auth_json(
        "u@e.com",
        chrono::Utc::now().timestamp() + 3600,
        "at",
        "rt",
        Some(&chrono::Utc::now().to_rfc3339()),
    );
    std::fs::write(&auth_path, &fresh).unwrap();
    let mtime_before = std::fs::metadata(&auth_path).unwrap().modified().unwrap();

    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // No mock server registered at all — a network call here would fail
        // fast with a connection error, proving grant_needs_refresh's
        // short-circuit ran before any I/O beyond the initial read.
        let outcome = active_refresh_cas(900, 8 * 86400, None).unwrap();
        assert_eq!(outcome, CasOutcome::Fresh);
    });

    let on_disk = std::fs::read_to_string(&auth_path).unwrap();
    assert_eq!(
        on_disk, fresh,
        "a Fresh outcome must never rewrite auth.json"
    );
    let mtime_after = std::fs::metadata(&auth_path).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after);
}

#[test]
fn codex_active_refresh_cas_adopts_when_last_known_blob_already_stale() {
    let _g = crate::store::ScopedConfigDir::new();
    // If the caller's cached reference doesn't match what's on disk BEFORE
    // we even check whether a refresh is due, someone already rotated the
    // file behind our back — adopt it immediately, no network call at all.
    let dir = tempfile::tempdir().unwrap();
    let current = make_auth_json(
        "u@e.com",
        chrono::Utc::now().timestamp() + 3600,
        "at",
        "rt",
        None,
    );
    std::fs::write(dir.path().join("auth.json"), &current).unwrap();

    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let outcome = active_refresh_cas(900, 8 * 86400, Some("stale-cached-blob")).unwrap();
        match outcome {
            CasOutcome::Adopted(blob) => assert_eq!(blob, current),
            other => panic!("expected Adopted, got {other:?}"),
        }
    });
}

#[test]
fn active_refresh_cas_errors_without_refresh_token() {
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

    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let err = active_refresh_cas(900, 8 * 86400, None).unwrap_err();
        assert!(matches!(err, RefreshError::InvalidGrant));
    });
}

// --- apply_grant_to_blob -----------------------------------------------------

#[test]
fn apply_grant_to_blob_preserves_unrelated_fields_and_updates_id_token() {
    let blob = serde_json::json!({
        "OPENAI_API_KEY": serde_json::Value::Null,
        "tokens": {
            "access_token": "old-at",
            "refresh_token": "old-rt",
            "id_token": "old-id",
            "account_id": "acc-1",
        }
    })
    .to_string();
    let grant = CodexRefreshGrant {
        access_token: "new-at".into(),
        refresh_token: "new-rt".into(),
        id_token: Some("new-id".into()),
        expires_in_secs: 3600,
    };
    let out = apply_grant_to_blob(&blob, &grant).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["tokens"]["access_token"], "new-at");
    assert_eq!(v["tokens"]["refresh_token"], "new-rt");
    assert_eq!(v["tokens"]["id_token"], "new-id");
    // account_id (untouched field) survives.
    assert_eq!(v["tokens"]["account_id"], "acc-1");
    assert!(v["last_refresh"].as_str().is_some());
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
