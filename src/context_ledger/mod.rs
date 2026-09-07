//! Context Ledger — audit what an AI CLI auto-injects into the model's
//! context per turn, with token cost per item.
//!
//! Two consumer surfaces:
//! 1. CLI: `usagio context [--provider <slug>] [--project <path>]`
//! 2. Menu: "Context Ledger ▸" floating NSPanel (deferred to a follow-up commit)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod claude;
pub mod cli;
pub mod codex;
pub mod mcp;
pub mod opencode;
pub mod render;
pub mod tokenize;

#[cfg(test)]
mod tests;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    GlobalInstructions,
    ProjectInstructions,
    Skill,
    McpTools,
    SubagentDef,
    PluginManifest,
    /// Free-form context extension (e.g. opencode `prompt` string).
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerItem {
    pub kind: ItemKind,
    pub name: String,
    pub path: PathBuf,
    /// mtime at capture time (for staleness detection).
    pub mtime: DateTime<Utc>,
    pub token_count: usize,
    pub content_bytes: usize,
    /// Optional MCP-server-specific: how many tools contribute.
    pub tool_count: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ledger {
    pub provider: String,
    pub project: Option<PathBuf>,
    pub items: Vec<LedgerItem>,
    pub total_tokens: usize,
    pub estimated_cost_per_turn: Option<f64>,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcp: {0}")]
    Mcp(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Build the ledger for a provider. `project` is a filesystem path the CLI
/// would be invoked from — used to find project-scoped context (e.g. an
/// in-tree CLAUDE.md). Missing config dirs yield an empty ledger, not an error.
pub fn build_ledger(provider: &str, project: Option<&Path>) -> Result<Ledger, LedgerError> {
    let items = match provider {
        "claude" | "claude-code" => claude::collect(project)?,
        "codex" => codex::collect(project)?,
        "opencode" => opencode::collect(project)?,
        other => return Err(LedgerError::UnknownProvider(other.to_string())),
    };
    let total_tokens = items.iter().map(|i| i.token_count).sum();
    let estimated_cost_per_turn = estimate_cost(provider, total_tokens);
    Ok(Ledger {
        provider: provider.to_string(),
        project: project.map(|p| p.to_path_buf()),
        items,
        total_tokens,
        estimated_cost_per_turn,
        captured_at: Utc::now(),
    })
}

pub fn render_terminal(ledger: &Ledger) -> String {
    render::terminal(ledger)
}

/// Returns true if any tracked file's on-disk mtime is newer than captured_at.
/// Missing files are ignored (returns false for that item).
pub fn is_stale(ledger: &Ledger) -> bool {
    ledger.items.iter().any(|item| {
        std::fs::metadata(&item.path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mt| DateTime::<Utc>::from(mt).signed_duration_since(ledger.captured_at).num_milliseconds().checked_abs())
            .map(|diff_ms| diff_ms > 0 && DateTime::<Utc>::from(std::fs::metadata(&item.path).unwrap().modified().unwrap()) > ledger.captured_at)
            .unwrap_or(false)
    })
}

/// Rough cost estimate per turn — very approximate. Uses Sonnet input pricing
/// as a default; real per-model math lives in a future pricing module.
fn estimate_cost(provider: &str, total_tokens: usize) -> Option<f64> {
    let per_million_usd = match provider {
        "claude" | "claude-code" => 3.0,   // Sonnet input, USD per 1M tokens
        "codex" => 5.0,                    // GPT-4.1 input ballpark
        "opencode" => 3.0,                 // typically Anthropic-backed
        _ => return None,
    };
    Some((total_tokens as f64) * per_million_usd / 1_000_000.0)
}
