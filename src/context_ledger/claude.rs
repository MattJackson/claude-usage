//! Discovery for the Anthropic Claude Code CLI.
//!
//! Claude loads context from a well-documented set of locations:
//! - ~/.claude/CLAUDE.md — global instructions
//! - <project>/CLAUDE.md — project instructions
//! - ~/.claude/skills/*/SKILL.md — skill definitions (auto-loaded)
//! - ~/.claude/agents/*.md — subagent definitions
//! - ~/.claude/plugins/**/plugin.json — plugin manifests
//! - ~/.claude/settings.json — mcpServers → each server contributes its tool schemas

use super::{mcp, tokenize, ItemKind, LedgerError, LedgerItem};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(items),
    };
    let claude_dir = home.join(".claude");

    // Global CLAUDE.md
    push_file_if_exists(
        &mut items,
        claude_dir.join("CLAUDE.md"),
        ItemKind::GlobalInstructions,
        "Global CLAUDE.md",
    );

    // Project CLAUDE.md
    if let Some(proj) = project {
        push_file_if_exists(
            &mut items,
            proj.join("CLAUDE.md"),
            ItemKind::ProjectInstructions,
            "Project CLAUDE.md",
        );
    }

    // Skills — ~/.claude/skills/<name>/SKILL.md
    let skills_dir = claude_dir.join("skills");
    if let Ok(entries) = fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let skill_md = entry.path().join("SKILL.md");
            push_file_if_exists(
                &mut items,
                skill_md,
                ItemKind::Skill,
                &format!("skill: {}", name),
            );
        }
    }

    // Subagents — ~/.claude/agents/*.md
    let agents_dir = claude_dir.join("agents");
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

    // Plugins — ~/.claude/plugins/**/plugin.json (only depth 2 to avoid runaway walks)
    let plugins_dir = claude_dir.join("plugins");
    if let Ok(top) = fs::read_dir(&plugins_dir) {
        for entry in top.flatten() {
            let manifest = entry.path().join("plugin.json");
            if manifest.exists() {
                let name = entry.file_name().to_string_lossy().into_owned();
                push_file_if_exists(
                    &mut items,
                    manifest,
                    ItemKind::PluginManifest,
                    &format!("plugin: {}", name),
                );
            }
        }
    }

    // MCP tools — parse settings.json for mcpServers block, then fetch schemas
    let settings = claude_dir.join("settings.json");
    if settings.exists() {
        if let Ok(bytes) = fs::read(&settings) {
            if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let servers = val
                    .get("mcpServers")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                for (name, config) in servers {
                    match mcp::fetch_tools(&config) {
                        Ok(summary) => {
                            items.push(LedgerItem {
                                kind: ItemKind::McpTools,
                                name: format!("mcp: {}", name),
                                path: settings.clone(),
                                mtime: file_mtime(&settings),
                                token_count: summary.token_count,
                                content_bytes: summary.byte_count,
                                tool_count: Some(summary.tool_count),
                            });
                        }
                        Err(err) => {
                            // Skip failed MCP servers — timeout / crash / bad config.
                            // In a follow-up we surface a "?" row with the error.
                            eprintln!("mcp {} skipped: {}", name, err);
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
