//! Terminal renderer for the ledger — column-aligned tree with totals.

use super::{ItemKind, Ledger};

pub fn terminal(ledger: &Ledger) -> String {
    let mut out = String::new();
    let project = ledger
        .project
        .as_ref()
        .map(|p| format!(", project: {}", p.display()))
        .unwrap_or_default();
    out.push_str(&format!(
        "Context Ledger — {}{}\n",
        display_provider(&ledger.provider),
        project,
    ));

    if ledger.items.is_empty() {
        out.push_str("  (no context items discovered)\n");
        return out;
    }

    // Group by kind for readability.
    let ordered = [
        ItemKind::GlobalInstructions,
        ItemKind::ProjectInstructions,
        ItemKind::Skill,
        ItemKind::SubagentDef,
        ItemKind::PluginManifest,
        ItemKind::McpTools,
        ItemKind::Other,
    ];

    // Column width for the name so token counts align.
    let name_col = ledger
        .items
        .iter()
        .map(|i| i.name.chars().count())
        .max()
        .unwrap_or(20)
        .max(20);

    for kind in ordered {
        let group: Vec<_> = ledger.items.iter().filter(|i| i.kind == kind).collect();
        if group.is_empty() {
            continue;
        }
        let subtotal: usize = group.iter().map(|i| i.token_count).sum();
        out.push_str(&format!(
            "  {:name_col$}   {:>10} tok\n",
            display_kind(kind),
            fmt_thousands(subtotal),
            name_col = name_col,
        ));
        for item in group {
            let tool_hint = item
                .tool_count
                .map(|n| format!(" ({} tools)", n))
                .unwrap_or_default();
            out.push_str(&format!(
                "    {:name_col$}{:>10} tok{}\n",
                item.name,
                fmt_thousands(item.token_count),
                tool_hint,
                name_col = name_col.saturating_sub(2),
            ));
        }
    }

    out.push_str(&format!(
        "  {:-<width$}\n",
        "",
        width = name_col + 20
    ));
    out.push_str(&format!(
        "  {:name_col$}   {:>10} tok\n",
        "Total baseline per turn",
        fmt_thousands(ledger.total_tokens),
        name_col = name_col,
    ));
    if let Some(cost) = ledger.estimated_cost_per_turn {
        out.push_str(&format!(
            "  {:name_col$}   ${:>9.4}\n",
            "Est. cost per turn",
            cost,
            name_col = name_col,
        ));
    }
    out
}

fn display_provider(slug: &str) -> &str {
    match slug {
        "claude" | "claude-code" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "opencode",
        other => other,
    }
}

fn display_kind(k: ItemKind) -> &'static str {
    match k {
        ItemKind::GlobalInstructions => "Global instructions",
        ItemKind::ProjectInstructions => "Project instructions",
        ItemKind::Skill => "Skills",
        ItemKind::SubagentDef => "Subagents",
        ItemKind::PluginManifest => "Plugins",
        ItemKind::McpTools => "MCP servers",
        ItemKind::Other => "Other",
    }
}

fn fmt_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}
