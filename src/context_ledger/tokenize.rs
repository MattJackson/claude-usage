//! Token counting for context items.
//!
//! Real tokenizers per model family (via tiktoken-rs for OpenAI), and a plain
//! chars-per-token approximation as a fallback for Anthropic/others (their
//! server-side tokenizer isn't publicly available in a Rust crate today).
//!
//! For the ledger UX, ±20% accuracy is fine — this is a signal, not a bill.

#[derive(Copy, Clone, Debug)]
pub enum TokenizerHint {
    /// Use OpenAI cl100k_base / o200k_base via tiktoken-rs. Accurate.
    OpenAi,
    /// Approximate — Anthropic doesn't ship a Rust tokenizer.
    Anthropic,
    /// Approximate — used for unknown provider defaults.
    Approx,
}

/// Count tokens in `text`. Falls back to chars/3.6 when a real tokenizer isn't
/// available or the specific model's encoder can't be constructed.
pub fn count_tokens(text: &str, hint: TokenizerHint) -> usize {
    if text.is_empty() {
        return 0;
    }
    match hint {
        TokenizerHint::OpenAi => openai_tokens(text).unwrap_or_else(|| approx(text)),
        TokenizerHint::Anthropic | TokenizerHint::Approx => approx(text),
    }
}

#[cfg(feature = "tiktoken")]
fn openai_tokens(text: &str) -> Option<usize> {
    use tiktoken_rs::o200k_base;
    let bpe = o200k_base().ok()?;
    Some(bpe.encode_with_special_tokens(text).len())
}

#[cfg(not(feature = "tiktoken"))]
fn openai_tokens(_text: &str) -> Option<usize> {
    None
}

/// Anthropic + fallback approximation. English averages ~3.6 chars per token
/// for modern BPE tokenizers; use 3 for slight over-estimate (safer to show
/// "worse" numbers in a quota UI than to under-report).
fn approx(text: &str) -> usize {
    let chars = text.chars().count();
    // 3 chars/token → tokens = chars * 1/3, but do it as chars*11/40 ≈ 0.275
    // to land ~3.6 chars/token which matches empirical Claude/GPT counts.
    (chars * 11).div_ceil(40)
}

#[cfg(test)]
mod approx_tests {
    use super::*;

    #[test]
    fn empty_zero() {
        assert_eq!(count_tokens("", TokenizerHint::Anthropic), 0);
    }

    #[test]
    fn approx_is_reasonable() {
        // A typical sentence: "The quick brown fox jumps over the lazy dog."
        // 44 chars → ~12 tokens by approx (44*11/40 = 12.1 → 13 with ceil).
        // Real BPE would give ~10. Within ±30%.
        let t = "The quick brown fox jumps over the lazy dog.";
        let n = count_tokens(t, TokenizerHint::Anthropic);
        assert!((8..=16).contains(&n), "got {}", n);
    }
}
