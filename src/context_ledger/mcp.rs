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
//! Timeout: 3s per server. Missing binaries / crashes surface as errors and
//! the caller skips that row.

use super::tokenize;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RPC_TIMEOUT: Duration = Duration::from_secs(3);

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
    let cfg: StdioConfig = serde_json::from_value(config.clone())
        .map_err(|e| format!("bad stdio config: {}", e))?;

    let start = Instant::now();
    let mut child = Command::new(&cfg.command)
        .args(&cfg.args)
        .envs(&cfg.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn: {}", e))?;

    let stdin = child.stdin.as_mut().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut reader = BufReader::new(stdout);

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
    writeln!(stdin, "{}", init).map_err(|e| format!("write init: {}", e))?;

    // Wait for initialize response
    let _init_response = read_line_with_timeout(&mut reader, start)?;

    // Send initialized notification (no id — no response expected)
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(stdin, "{}", initialized).map_err(|e| format!("write initialized: {}", e))?;

    // tools/list
    let tools_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    writeln!(stdin, "{}", tools_req).map_err(|e| format!("write tools/list: {}", e))?;

    let tools_response = read_line_with_timeout(&mut reader, start)?;
    let parsed: Value =
        serde_json::from_str(&tools_response).map_err(|e| format!("parse tools/list: {}", e))?;
    let tools = parsed
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .ok_or("tools/list missing result.tools")?
        .clone();

    // Best-effort cleanup — kill child, don't block on it exiting.
    let _ = child.kill();
    let _ = child.wait();

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
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => return Err("eof before response".into()),
            Ok(_) => {
                let trimmed = buf.trim();
                if trimmed.is_empty() {
                    continue;
                }
                return Ok(buf);
            }
            Err(e) => return Err(format!("read: {}", e)),
        }
    }
}
