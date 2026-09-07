//! Discovery for the OpenAI Codex CLI.
//!
//! Codex context sources (from openai/codex repo inspection):
//! - AGENTS.md at repo root or ~/.codex/AGENTS.md — instruction file
//! - ~/.codex/instructions.md — global instructions (older name)
//! - ~/.codex/config.toml → any [mcp_servers.*] blocks
//! - <project>/AGENTS.md — project-scoped instructions
//!
//! Codex evolves quickly — recheck paths against upstream when re-verifying.

use super::{mcp, tokenize, ItemKind, LedgerError, LedgerItem};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return Ok(items);
    };
    let codex_dir = home.join(".codex");

    // Global AGENTS.md / instructions.md — whichever exists
    for (fname, kind, display) in [
        ("AGENTS.md", ItemKind::GlobalInstructions, "Global AGENTS.md"),
        ("instructions.md", ItemKind::GlobalInstructions, "Global instructions.md"),
    ] {
        push_file_if_exists(&mut items, codex_dir.join(fname), kind, display);
    }

    // Project AGENTS.md
    if let Some(proj) = project {
        push_file_if_exists(
            &mut items,
            proj.join("AGENTS.md"),
            ItemKind::ProjectInstructions,
            "Project AGENTS.md",
        );
    }

    // MCP servers from config.toml
    let config = codex_dir.join("config.toml");
    if config.exists() {
        if let Ok(text) = fs::read_to_string(&config) {
            if let Ok(parsed) = toml::from_str::<toml::Value>(&text) {
                if let Some(mcp_servers) = parsed.get("mcp_servers").and_then(|v| v.as_table()) {
                    for (name, server_cfg) in mcp_servers {
                        // Convert TOML to JSON for the mcp module's uniform interface.
                        let json =
                            serde_json::to_value(server_cfg).unwrap_or(serde_json::Value::Null);
                        match mcp::fetch_tools(&json) {
                            Ok(summary) => items.push(LedgerItem {
                                kind: ItemKind::McpTools,
                                name: format!("mcp: {}", name),
                                path: config.clone(),
                                mtime: file_mtime(&config),
                                token_count: summary.token_count,
                                content_bytes: summary.byte_count,
                                tool_count: Some(summary.tool_count),
                            }),
                            Err(err) => eprintln!("codex mcp {} skipped: {}", name, err),
                        }
                    }
                }
            }
        }
    }

    Ok(items)
}

fn push_file_if_exists(items: &mut Vec<LedgerItem>, path: PathBuf, kind: ItemKind, name: &str) {
    let Ok(bytes) = fs::read(&path) else { return };
    let text = String::from_utf8_lossy(&bytes);
    let token_count = tokenize::count_tokens(&text, tokenize::TokenizerHint::OpenAi);
    items.push(LedgerItem {
        kind,
        name: name.to_string(),
        path: path.clone(),
        mtime: file_mtime(&path),
        token_count,
        content_bytes: bytes.len(),
        tool_count: None,
    });
}

fn file_mtime(path: &Path) -> DateTime<Utc> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}
