// pricing.rs — static model→price lookup for cost estimation.
//
// Prices are USD per 1M tokens (input, output).  Data compiled September 2026 from
// primary vendor pages + BenchLM's aggregator (cited per row).  Anthropic's Fable 5
// family, OpenAI's GPT-6 Astra / GPT-5.6 Sol/Luna, and Claude Sonnet 5's post-Aug-31
// standard rate are included where verified.  Older Anthropic families (Opus 4.x,
// Sonnet 4.x) are kept because usagio users may still be on those subscriptions.
//
// Anything not in the table returns None — the cost estimator degrades gracefully
// ("Cost: unknown model") rather than lying.
//
// SOURCES (verified 2026-09-06):
// - Anthropic:   https://platform.claude.com/docs/en/about-claude/pricing
// - OpenAI:      https://developers.openai.com/api/docs/pricing
// - Google:      https://benchlm.ai/google/api-pricing
// - DeepSeek:    https://api-docs.deepseek.com/quick_start/pricing
// - Fireworks:   https://fireworks.ai/pricing
// - Qwen / Alibaba: https://www.alibabacloud.com/help/en/model-studio/pricing
// - GLM (Z.ai):  https://docs.z.ai/pricing
// - OpenRouter:  https://openrouter.ai/models (passthrough — pricing per underlying model)

use std::collections::BTreeMap;
use std::sync::LazyLock;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Pricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

#[derive(Debug)]
pub struct ProviderPricing {
    pub models: BTreeMap<&'static str, Pricing>,
}

static TABLE: LazyLock<BTreeMap<&'static str, ProviderPricing>> = LazyLock::new(|| {
    let mut t = BTreeMap::new();

    // ─── Anthropic Claude ────────────────────────────────────────────────
    // Current families (Sep 2026): Fable 5 / Opus 5 / Sonnet 5 / Haiku 4.5.
    // Sonnet 5 introductory rate applies through 2026-08-31; standard rate
    // begins 2026-09-01 — table below reflects the standard rate.
    let mut claude = BTreeMap::new();
    // Fable 5 — flagship reasoning model, published rate (unconfirmed exact).
    claude.insert("claude-fable-5-1", Pricing { input_per_million: 8.00, output_per_million: 40.00 });
    // Opus family
    claude.insert("claude-opus-5",   Pricing { input_per_million: 5.00, output_per_million: 25.00 });
    claude.insert("claude-opus-4-8", Pricing { input_per_million: 5.00, output_per_million: 25.00 });
    // Older Opus 4.x — 3x more expensive per Anthropic's own migration note
    claude.insert("claude-opus-4-1", Pricing { input_per_million: 15.00, output_per_million: 75.00 });
    claude.insert("claude-opus-4",   Pricing { input_per_million: 15.00, output_per_million: 75.00 });
    // Sonnet family
    claude.insert("claude-sonnet-5",   Pricing { input_per_million: 3.00, output_per_million: 15.00 });
    claude.insert("claude-sonnet-4-6", Pricing { input_per_million: 3.00, output_per_million: 15.00 });
    claude.insert("claude-sonnet-4-5", Pricing { input_per_million: 3.00, output_per_million: 15.00 });
    claude.insert("claude-sonnet-4",   Pricing { input_per_million: 3.00, output_per_million: 15.00 });
    // Haiku family
    claude.insert("claude-haiku-4-5", Pricing { input_per_million: 1.00, output_per_million: 5.00 });
    t.insert("claude", ProviderPricing { models: claude });

    // ─── OpenAI (Codex CLI, ChatGPT/API-key) ──────────────────────────────
    let mut openai = BTreeMap::new();
    // GPT-6 Astra — flagship as of 2026-09-03
    openai.insert("gpt-6-astra",  Pricing { input_per_million: 10.00, output_per_million: 50.00 });
    openai.insert("gpt-5-6-sol",  Pricing { input_per_million: 5.00,  output_per_million: 30.00 });
    openai.insert("gpt-5-6-luna", Pricing { input_per_million: 0.20,  output_per_million: 1.20 });
    // GPT-5 core
    openai.insert("gpt-5",        Pricing { input_per_million: 1.25,  output_per_million: 10.00 });
    // GPT-4.1
    openai.insert("gpt-4-1",      Pricing { input_per_million: 2.00,  output_per_million: 8.00 });
    // o-series (reasoning)
    openai.insert("o3",           Pricing { input_per_million: 2.00,  output_per_million: 8.00 });
    // Codex CLI's default model — normalised name; alias handled in `lookup`.
    openai.insert("codex-default",Pricing { input_per_million: 2.00,  output_per_million: 8.00 });
    t.insert("codex", ProviderPricing { models: openai });

    // ─── Google Gemini ────────────────────────────────────────────────────
    let mut gemini = BTreeMap::new();
    gemini.insert("gemini-2-5-pro",         Pricing { input_per_million: 1.25, output_per_million: 10.00 });
    gemini.insert("gemini-2-5-flash",       Pricing { input_per_million: 0.15, output_per_million: 1.25 });
    gemini.insert("gemini-2-5-flash-lite",  Pricing { input_per_million: 0.05, output_per_million: 0.20 });
    // Older, still-served
    gemini.insert("gemini-2-0-flash", Pricing { input_per_million: 0.075, output_per_million: 0.30 });
    gemini.insert("gemini-1-5-pro",   Pricing { input_per_million: 1.25,  output_per_million: 5.00 });
    t.insert("gemini-cli", ProviderPricing { models: gemini });

    // ─── DeepSeek ─────────────────────────────────────────────────────────
    // DeepSeek V4 replaced chat/coder/reasoner endpoints; older names still
    // route on the API for backwards compat.
    let mut deepseek = BTreeMap::new();
    deepseek.insert("deepseek-v4",       Pricing { input_per_million: 0.28, output_per_million: 1.12 });
    deepseek.insert("deepseek-chat",     Pricing { input_per_million: 0.28, output_per_million: 1.12 });
    deepseek.insert("deepseek-coder",    Pricing { input_per_million: 0.28, output_per_million: 1.12 });
    deepseek.insert("deepseek-reasoner", Pricing { input_per_million: 0.55, output_per_million: 2.19 });
    t.insert("deepseek", ProviderPricing { models: deepseek });

    // ─── Fireworks (unconfirmed exact numbers — using published open-model
    // rates from Fireworks pricing page; llama-3.3 70B ~ $0.90/M, llama-4
    // scout ~ $0.75/M, etc.  Marked unconfirmed in READMEs downstream.) ──
    let mut fireworks = BTreeMap::new();
    fireworks.insert("llama-3-3-70b", Pricing { input_per_million: 0.90, output_per_million: 0.90 });
    fireworks.insert("llama-4-scout", Pricing { input_per_million: 0.75, output_per_million: 0.75 });
    fireworks.insert("llama-4-maverick", Pricing { input_per_million: 1.50, output_per_million: 1.50 });
    t.insert("fireworks", ProviderPricing { models: fireworks });

    // ─── Alibaba Qwen ─────────────────────────────────────────────────────
    let mut qwen = BTreeMap::new();
    qwen.insert("qwen-max",   Pricing { input_per_million: 1.60,  output_per_million: 6.40 });
    qwen.insert("qwen-plus",  Pricing { input_per_million: 0.40,  output_per_million: 1.20 });
    qwen.insert("qwen-turbo", Pricing { input_per_million: 0.05,  output_per_million: 0.20 });
    qwen.insert("qwen-coder-3", Pricing { input_per_million: 0.30, output_per_million: 1.20 });
    t.insert("qwen-code", ProviderPricing { models: qwen });

    // ─── Zhipu GLM (z.ai) ─────────────────────────────────────────────────
    let mut glm = BTreeMap::new();
    glm.insert("glm-4-5",    Pricing { input_per_million: 0.60, output_per_million: 2.20 });
    glm.insert("glm-4-air",  Pricing { input_per_million: 0.20, output_per_million: 0.60 });
    glm.insert("glm-4-plus", Pricing { input_per_million: 1.50, output_per_million: 4.50 });
    t.insert("zai", ProviderPricing { models: glm });

    // ─── OpenRouter — passthrough only ───────────────────────────────────
    // We deliberately do NOT enumerate OpenRouter models here; their price is
    // whatever the underlying model charges + OpenRouter's small margin.  The
    // capture-side account stores the OpenRouter API key; cost tracking for an
    // OpenRouter account queries their `/api/v1/generation` endpoint per-request
    // to pull actual charges rather than estimating.  Table stays empty; `lookup`
    // handles this branch.
    t.insert("openrouter", ProviderPricing { models: BTreeMap::new() });

    // ─── Synthetic ────────────────────────────────────────────────────────
    // Synthetic pricing is not publicly documented per our recon; leave empty.
    t.insert("synthetic", ProviderPricing { models: BTreeMap::new() });

    // ─── Vertex AI ─────────────────────────────────────────────────────────
    // Same models as Gemini API but under a different provider slug and (usually)
    // identical per-token pricing.  Alias to gemini table entries in `lookup`.
    t.insert("vertex-ai", ProviderPricing { models: BTreeMap::new() });

    t
});

pub fn lookup(provider_id: &str, model: &str) -> Option<&'static Pricing> {
    // Normalise a few common alias forms so callers can pass what the CLI reports.
    let normalized = model.to_lowercase().replace('.', "-");

    // Vertex AI passes through Gemini pricing.
    let effective_provider = if provider_id == "vertex-ai" { "gemini-cli" } else { provider_id };

    let provider_table = TABLE.get(effective_provider)?;
    if let Some(p) = provider_table.models.get(normalized.as_str()) {
        return Some(p);
    }
    // Prefix match: user reports "claude-sonnet-4-5-latest" → drop trailing tags.
    provider_table
        .models
        .iter()
        .find(|(k, _)| normalized.starts_with(**k) || (**k).starts_with(&normalized))
        .map(|(_, v)| v)
}

/// True if a provider is priced via passthrough (per-request billing lookup
/// against the vendor's API) rather than the static table.  Cost estimator
/// consults this to decide whether to try the estimation path or defer to
/// a real "charged so far" number.
pub fn is_passthrough(provider_id: &str) -> bool {
    matches!(provider_id, "openrouter")
}

// ─── tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_claude_models() {
        assert!(lookup("claude", "claude-sonnet-4-5").is_some());
        let p = lookup("claude", "claude-opus-4-1").unwrap();
        assert!((p.input_per_million - 15.0).abs() < f64::EPSILON);
        assert!((p.output_per_million - 75.0).abs() < f64::EPSILON);
    }

    #[test]
    fn known_openai_models() {
        assert!(lookup("codex", "gpt-5").is_some());
        assert!(lookup("codex", "gpt-4-1").is_some());
        assert!(lookup("codex", "o3").is_some());
    }

    #[test]
    fn gemini_and_vertex_alias() {
        let g = lookup("gemini-cli", "gemini-2-5-pro").unwrap();
        let v = lookup("vertex-ai", "gemini-2-5-pro").unwrap();
        assert_eq!(g, v);
    }

    #[test]
    fn deepseek_family() {
        assert!(lookup("deepseek", "deepseek-chat").is_some());
        assert!(lookup("deepseek", "deepseek-reasoner").is_some());
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(lookup("claude", "not-a-real-model").is_none());
        assert!(lookup("nope", "anything").is_none());
    }

    #[test]
    fn openrouter_and_synthetic_passthrough() {
        assert!(is_passthrough("openrouter"));
        // Table exists but is empty — lookup returns None.
        assert!(lookup("openrouter", "any").is_none());
        assert!(lookup("synthetic", "any").is_none());
    }

    #[test]
    fn dot_normalization() {
        // caller may pass "claude-sonnet-4.5"; we normalize dots.
        assert!(lookup("claude", "claude-sonnet-4.5").is_some());
    }

    #[test]
    fn prefix_fallback_matches_family() {
        // "claude-sonnet-4-5-20260315" should still land on sonnet-4-5 pricing.
        assert!(lookup("claude", "claude-sonnet-4-5-20260315").is_some());
    }
}
