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
//! Timeout: a hard wall-clock ~3s per server, enforced regardless of whether
//! the child ever writes a byte (robustness-02, v0.5.2 audit — see
//! `read_line_with_timeout`'s doc for why a plain polling loop around a
//! blocking `read_until` isn't enough). Missing binaries / crashes surface as
//! errors and the caller skips that row.

use super::tokenize;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
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
            // Kill the child's whole process GROUP, not just the direct child.
            // MCP servers are commonly `npx`/`node` (or `uvx`/`python`) wrappers
            // that spawn their own children; `Child::kill` signals only the
            // direct child, leaving those grandchildren orphaned and running
            // (H5 reaped the direct child but not its tree). The child is
            // spawned with `process_group(0)` on unix, so its PGID == its PID;
            // sending SIGKILL to `-PID` reaps the entire subtree. The direct
            // `kill()`/`wait()` still runs to reap the child itself (and is the
            // only cleanup on non-unix, where process groups aren't set up).
            #[cfg(unix)]
            {
                let pgid = c.id() as i32;
                // SAFETY: kill(2) with a negative pid targets that process
                // group. `pgid` is this child's own pid — which is its pgid
                // because it was spawned with process_group(0) — so this can
                // only ever signal processes usagio itself spawned.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
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
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .envs(&cfg.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Put the server in its own process group so ChildGuard's Drop can SIGKILL
    // the whole tree (npx → node → real server), not just the direct child.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {}", e))?;

    // Pull the stdio handles out BEFORE handing the child to ChildGuard so the
    // guard doesn't need to be re-borrowed for each I/O op (avoids a two-mut
    // borrow through `guard.as_mut()`). ChildGuard now owns kill+reap on any
    // early return between here and the happy-path drop at fn end.
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = BufReader::new(stdout);
    let _guard = ChildGuard::new(child);

    // robustness-02 (v0.5.2 audit): hand the blocking reader off to a
    // dedicated thread so the 3s budget below is a real wall-clock timeout,
    // not just a gap check between blocking `read_until` calls. A server
    // that writes nothing and never exits would otherwise block
    // `read_until` inside the kernel indefinitely — the old polling loop's
    // `start.elapsed() > RPC_TIMEOUT` check can only run BETWEEN calls, so it
    // never fires while a call is in flight. The reader thread itself
    // terminates naturally once `_guard`'s `Drop` kills the child: killing
    // it closes the write end of the stdout pipe, so the blocked
    // `read_until` unblocks with EOF instead of leaking the thread.
    let rx = spawn_line_reader(reader);

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
    let init_response = read_line_with_timeout(&rx, start)?;
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

    let tools_response = read_line_with_timeout(&rx, start)?;
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

/// One outcome of the background reader thread's attempt to produce the next
/// non-empty line.
enum LineMsg {
    Line(String),
    Eof,
    Err(String),
}

/// Spawn a thread that owns `reader` for the rest of the child's lifetime,
/// blocking on `read_until(b'\n')` in a loop and forwarding each non-empty
/// line (or the terminal EOF/error) over the returned channel. This is what
/// lets `read_line_with_timeout` enforce a real wall-clock deadline: the
/// blocking syscall lives on this thread, so the caller's `recv_timeout` can
/// give up on it without waiting for the kernel read to return.
fn spawn_line_reader(mut reader: BufReader<ChildStdout>) -> mpsc::Receiver<LineMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        // R3-RES-01: cap per-line reads at MAX_LINE_BYTES so a server that
        // streams bytes without a newline can't grow this buffer unboundedly.
        let mut bytes: Vec<u8> = Vec::new();
        let mut limited = reader.by_ref().take(MAX_LINE_BYTES);
        let msg = match limited.read_until(b'\n', &mut bytes) {
            Ok(0) => {
                let _ = tx.send(LineMsg::Eof);
                return;
            }
            Ok(_) => {
                if bytes.len() as u64 >= MAX_LINE_BYTES && !bytes.ends_with(b"\n") {
                    let _ = tx.send(LineMsg::Err(format!(
                        "line exceeded {} bytes without newline",
                        MAX_LINE_BYTES
                    )));
                    return;
                }
                match String::from_utf8(bytes) {
                    Ok(s) if s.trim().is_empty() => continue,
                    Ok(s) => LineMsg::Line(s),
                    Err(e) => {
                        let _ = tx.send(LineMsg::Err(format!("non-utf8 in response: {}", e)));
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(LineMsg::Err(format!("read: {}", e)));
                return;
            }
        };
        // The receiver may already be gone (fetch_tools returned early on a
        // prior error and dropped `rx`) — stop reading rather than spin
        // forever against a child nobody is listening to anymore. The
        // ChildGuard has already killed the process by the time that
        // happens, so this thread exits promptly either way.
        if tx.send(msg).is_err() {
            return;
        }
    });
    rx
}

/// Wait for the next line the background reader thread (see
/// `spawn_line_reader`) produces, enforcing a hard wall-clock deadline of
/// `RPC_TIMEOUT` measured from `start` — regardless of whether the child
/// ever writes anything. Unlike a polling loop around a blocking read, this
/// deadline is real: `recv_timeout` returns on schedule even while the
/// reader thread's `read_until` is still parked in the kernel waiting on the
/// child's stdout pipe.
fn read_line_with_timeout(rx: &mpsc::Receiver<LineMsg>, start: Instant) -> Result<String, String> {
    let remaining = RPC_TIMEOUT
        .checked_sub(start.elapsed())
        .unwrap_or(Duration::ZERO);
    if remaining.is_zero() {
        return Err(format!("timeout after {:?}", RPC_TIMEOUT));
    }
    match rx.recv_timeout(remaining) {
        Ok(LineMsg::Line(s)) => Ok(s),
        Ok(LineMsg::Eof) => Err("eof before response".into()),
        Ok(LineMsg::Err(e)) => Err(e),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!("timeout after {:?}", RPC_TIMEOUT)),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("reader thread ended without a response".into())
        }
    }
}
