//! MCP (Model Context Protocol) server tool-schema fetch.
//!
//! Spawn each server as configured, drive stdio JSON-RPC:
//!   1. `initialize` (required by spec before any other call)
//!   2. `notifications/initialized`
//!   3. `tools/list`
//!
//! Serialize the tools response to JSON and count its tokens — that's
//! approximately what enters the model's context per turn.
//!
//! Timeout: best-effort ~3s per server (a blocking `read_line` between polls
//! may exceed it if the child stops emitting bytes). Missing binaries /
//! crashes surface as errors and the caller skips that row.

use super::tokenize;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// RAII guard that kills + reaps a spawned MCP child on drop. Ensures every
/// `?` early return between spawn and cleanup — spawn failure, write failure,
/// timeout, malformed response — reaps the child rather than leaking it as a
/// zombie. See H5 in the round-1 codeaudit findings.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(c: Child) -> Self {
        Self(Some(c))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

const RPC_TIMEOUT: Duration = Duration::from_secs(3);

/// R3-RES-01: per-line byte cap for stdio JSON-RPC reads. A misbehaving MCP
/// server that streams bytes without a newline would otherwise grow the read
/// buffer unboundedly (BufRead::read_line has no size limit and the 3s
/// timeout is only checked between successive read_line calls). 1 MiB sits
/// well above realistic tools/list responses.
const MAX_LINE_BYTES: u64 = 1 << 20;

#[derive(Debug)]
pub struct McpSummary {
    pub tool_count: usize,
    pub token_count: usize,
    pub byte_count: usize,
}

#[derive(Debug, Deserialize)]
struct StdioConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
}

pub fn fetch_tools(config: &Value) -> Result<McpSummary, String> {
    // Only stdio-transport servers supported here. HTTP transport (URL-based)
    // is out of scope for the first ledger pass — flag and skip.
    if config.get("url").is_some() {
        return Err("http transport not yet supported".into());
    }
    let cfg: StdioConfig =
        serde_json::from_value(config.clone()).map_err(|e| format!("bad stdio config: {}", e))?;

    let start = Instant::now();
    let mut child = Command::new(&cfg.command)
        .args(&cfg.args)
        .envs(&cfg.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn: {}", e))?;

    // Pull the stdio handles out BEFORE handing the child to ChildGuard so the
    // guard doesn't need to be re-borrowed for each I/O op (avoids a two-mut
    // borrow through `guard.as_mut()`). ChildGuard now owns kill+reap on any
    // early return between here and the happy-path drop at fn end.
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut reader = BufReader::new(stdout);
    let _guard = ChildGuard::new(child);

    // Initialize request
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "usagio-ledger", "version": "0.1"}
        }
    });
    writeln!(&mut stdin, "{}", init).map_err(|e| format!("write init: {}", e))?;

    // Wait for initialize response; if the server rejected our handshake
    // (JSON-RPC error object at top level), surface it now instead of
    // proceeding into a `tools/list` that would fail the same way.
    let init_response = read_line_with_timeout(&mut reader, start)?;
    if let Ok(parsed) = serde_json::from_str::<Value>(&init_response) {
        if let Some(err) = parsed.get("error") {
            return Err(format!("initialize failed: {}", err));
        }
    }

    // Send initialized notification (no id — no response expected)
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(&mut stdin, "{}", initialized).map_err(|e| format!("write initialized: {}", e))?;

    // tools/list
    let tools_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    writeln!(&mut stdin, "{}", tools_req).map_err(|e| format!("write tools/list: {}", e))?;

    let tools_response = read_line_with_timeout(&mut reader, start)?;
    let parsed: Value =
        serde_json::from_str(&tools_response).map_err(|e| format!("parse tools/list: {}", e))?;
    let tools = parsed
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .ok_or("tools/list missing result.tools")?
        .clone();

    // Happy path: ChildGuard::Drop at end-of-scope will kill+reap the child.
    let serialized = serde_json::to_string(&tools).unwrap_or_default();
    let token_count = tokenize::count_tokens(&serialized, tokenize::TokenizerHint::Anthropic);
    Ok(McpSummary {
        tool_count: tools.len(),
        token_count,
        byte_count: serialized.len(),
    })
}

fn read_line_with_timeout(
    reader: &mut BufReader<std::process::ChildStdout>,
    start: Instant,
) -> Result<String, String> {
    // Polling read: check elapsed each iteration, bail if we're over budget.
    // Not perfect (blocking read_line can hang beyond timeout), but pragmatic
    // for the ledger's "skip on trouble" contract.
    loop {
        if start.elapsed() > RPC_TIMEOUT {
            return Err(format!("timeout after {:?}", RPC_TIMEOUT));
        }
        // R3-RES-01: cap per-line reads at MAX_LINE_BYTES so a server that
        // streams bytes without a newline can't grow this buffer unboundedly
        // between timeout polls.
        let mut bytes: Vec<u8> = Vec::new();
        let mut limited = reader.by_ref().take(MAX_LINE_BYTES);
        match limited.read_until(b'\n', &mut bytes) {
            Ok(0) => return Err("eof before response".into()),
            Ok(_) => {
                if bytes.len() as u64 >= MAX_LINE_BYTES && !bytes.ends_with(b"\n") {
                    return Err(format!(
                        "line exceeded {} bytes without newline",
                        MAX_LINE_BYTES
                    ));
                }
                let s =
                    String::from_utf8(bytes).map_err(|e| format!("non-utf8 in response: {}", e))?;
                if s.trim().is_empty() {
                    continue;
                }
                return Ok(s);
            }
            Err(e) => return Err(format!("read: {}", e)),
        }
    }
}
