//! Discovery for the opencode CLI (SST).
//!
//! Opencode context sources:
//! - ~/.config/opencode/opencode.jsonc (or opencode.json) — top-level config
//!   with optional `prompt` / `rules` strings and `mcp` server map
//! - ~/.config/opencode/agent/*.md — subagent definitions
//! - <project>/AGENTS.md — sometimes read via project convention
//!
//! `.jsonc` may contain comments — strip line comments before parsing.

use super::{mcp, tokenize, ItemKind, LedgerError, LedgerItem};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return Ok(items);
    };
    let cfg_dir = home.join(".config").join("opencode");

    // opencode.jsonc / opencode.json
    let config_candidates = ["opencode.jsonc", "opencode.json"];
    let mut chosen_config = None;
    for name in config_candidates {
        let p = cfg_dir.join(name);
        if p.exists() {
            chosen_config = Some(p);
            break;
        }
    }

    if let Some(config_path) = chosen_config {
        if let Ok(raw) = fs::read_to_string(&config_path) {
            let stripped = strip_jsonc_comments(&raw);
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&stripped) {
                // `prompt` string
                if let Some(prompt) = val.get("prompt").and_then(|v| v.as_str()) {
                    let tokens = tokenize::count_tokens(prompt, tokenize::TokenizerHint::Anthropic);
                    items.push(LedgerItem {
                        kind: ItemKind::Other,
                        name: "opencode prompt".into(),
                        path: config_path.clone(),
                        mtime: file_mtime(&config_path),
                        token_count: tokens,
                        content_bytes: prompt.len(),
                        tool_count: None,
                    });
                }

                // `rules` array of strings
                if let Some(rules) = val.get("rules").and_then(|v| v.as_array()) {
                    let joined = rules
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !joined.is_empty() {
                        let tokens =
                            tokenize::count_tokens(&joined, tokenize::TokenizerHint::Anthropic);
                        items.push(LedgerItem {
                            kind: ItemKind::GlobalInstructions,
                            name: "opencode rules".into(),
                            path: config_path.clone(),
                            mtime: file_mtime(&config_path),
                            token_count: tokens,
                            content_bytes: joined.len(),
                            tool_count: None,
                        });
                    }
                }

                // `mcp` — server map, same shape as Claude's mcpServers
                if let Some(servers) = val.get("mcp").and_then(|v| v.as_object()) {
                    for (name, config) in servers {
                        match mcp::fetch_tools(config) {
                            Ok(summary) => items.push(LedgerItem {
                                kind: ItemKind::McpTools,
                                name: format!("mcp: {}", name),
                                path: config_path.clone(),
                                mtime: file_mtime(&config_path),
                                token_count: summary.token_count,
                                content_bytes: summary.byte_count,
                                tool_count: Some(summary.tool_count),
                            }),
                            Err(err) => eprintln!("opencode mcp {} skipped: {}", name, err),
                        }
                    }
                }
            }
        }
    }

    // Subagents — ~/.config/opencode/agent/*.md
    let agents_dir = cfg_dir.join("agent");
    if let Ok(entries) = fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "agent".to_string());
                push_file_if_exists(
                    &mut items,
                    path,
                    ItemKind::SubagentDef,
                    &format!("agent: {}", name),
                );
            }
        }
    }

    // Project AGENTS.md (opencode also reads this by convention on some paths)
    if let Some(proj) = project {
        push_file_if_exists(
            &mut items,
            proj.join("AGENTS.md"),
            ItemKind::ProjectInstructions,
            "Project AGENTS.md",
        );
    }

    Ok(items)
}

fn push_file_if_exists(items: &mut Vec<LedgerItem>, path: PathBuf, kind: ItemKind, name: &str) {
    let Ok(bytes) = fs::read(&path) else { return };
    let text = String::from_utf8_lossy(&bytes);
    let token_count = tokenize::count_tokens(&text, tokenize::TokenizerHint::Anthropic);
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

/// Minimal JSONC comment stripper. Handles `// line` and `/* block */` — good
/// enough for typical opencode configs; NOT a full spec-compliant JSONC parser
/// (does not respect string boundaries in edge cases like `"a // b"`).
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2 == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev = ' ';
                    for c2 in chars.by_ref() {
                        if prev == '*' && c2 == '/' {
                            break;
                        }
                        prev = c2;
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}
