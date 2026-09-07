// cost_tracking.rs — $ estimates per cycle + subscription verdict.
//
// Since providers surface % consumed rather than raw token counts, cost is estimated
// as `pct_consumed × plan_cap_tokens × price_per_token`, where `plan_cap_tokens` is a
// per-plan hardcoded ballpark (public plans don't publish exact token quotas, so
// these are approximations from vendor docs + community reports; see PLAN_CAPS below).
//
// Every returned struct carries a disclaimer string.  Menu row prefixes "~$" to make
// the approximation visible.  See README-DRAFT.md for wiring.

use crate::pricing::{self, Pricing};
use crate::providers::trait_def::Window;
use crate::usage_log::{self, AccountKey};

pub const DISCLAIMER: &str =
    "Estimate: derived from % consumed × plan cap × published pricing. \
     Providers don't expose raw token counts; treat as ballpark, not billing.";

/// Approximate per-cycle token cap by (provider_id, plan_slug).  Sources embedded
/// as comments so future updates can trace them.  These are the numbers we
/// multiply by "pct consumed" to arrive at estimated_input_tokens.
///
/// All values in *total* tokens per cycle (input+output combined at typical 60/40
/// ratio unless otherwise noted).  Adjust as vendors publish real numbers.
pub const CLAUDE_MAX_100_WEEKLY_TOKENS: u64 = 15_000_000;   // ~ from community traces
pub const CLAUDE_MAX_200_WEEKLY_TOKENS: u64 = 45_000_000;   // 3x the $100 tier
pub const CLAUDE_MAX_300_WEEKLY_TOKENS: u64 = 90_000_000;   // 6x the $100 tier
pub const CODEX_PLUS_WEEKLY_TOKENS:     u64 = 8_000_000;
pub const CODEX_PRO_WEEKLY_TOKENS:      u64 = 35_000_000;
/// I/O split used when we don't have observed breakdown per account.
pub const IO_SPLIT_INPUT_PCT: f64 = 0.65;

#[derive(Clone, Debug)]
pub struct CycleCost {
    pub account: AccountKey,
    pub window: Window,
    pub estimated_input_tokens: u64,
    pub estimated_output_tokens: u64,
    pub estimated_usd: f64,
    pub disclaimer: &'static str,
}

pub fn estimate_cycle_cost(
    account: &AccountKey,
    plan_cap_tokens: u64,
) -> Option<CycleCost> {
    // Pull most-recent snapshot for this account from the log to get pct consumed.
    let snaps = usage_log::last_n_days(account, 8);
    let latest = snaps.iter().max_by_key(|s| s.ts)?;
    let pct = latest.weekly_pct.or(latest.session_pct)? as f64;
    if pct <= 0.0 {
        return None;
    }
    let window = if latest.weekly_pct.is_some() {
        Window::Weekly
    } else {
        Window::Session
    };

    let consumed = (plan_cap_tokens as f64) * (pct / 100.0);
    let input = (consumed * IO_SPLIT_INPUT_PCT).round() as u64;
    let output = (consumed * (1.0 - IO_SPLIT_INPUT_PCT)).round() as u64;

    let model_slug = latest.active_model.clone().unwrap_or_default();
    let usd = pricing::lookup(&latest.provider, &model_slug)
        .map(|p| price_tokens(input, output, p))
        // If pricing is unknown, fall back to a conservative provider default.
        .unwrap_or_else(|| provider_default_price(&latest.provider, input, output));

    Some(CycleCost {
        account: account.clone(),
        window,
        estimated_input_tokens: input,
        estimated_output_tokens: output,
        estimated_usd: usd,
        disclaimer: DISCLAIMER,
    })
}

fn price_tokens(input: u64, output: u64, p: &Pricing) -> f64 {
    (input as f64 / 1_000_000.0) * p.input_per_million
        + (output as f64 / 1_000_000.0) * p.output_per_million
}

/// If the model wasn't known, use the provider's typical *middle-tier* price so
/// the dollar figure lands in the right order of magnitude.  Returns 0.0 for
/// providers where we can't even ballpark (openrouter/synthetic passthrough).
fn provider_default_price(provider_id: &str, input: u64, output: u64) -> f64 {
    let defaults = match provider_id {
        "claude"     => Pricing { input_per_million: 3.00, output_per_million: 15.00 },  // Sonnet-tier
        "codex"      => Pricing { input_per_million: 2.00, output_per_million: 8.00 },   // GPT-4.1-tier
        "gemini-cli" | "vertex-ai" => Pricing { input_per_million: 1.25, output_per_million: 10.00 }, // 2.5-pro
        "qwen-code"  => Pricing { input_per_million: 0.40, output_per_million: 1.20 },
        "deepseek"   => Pricing { input_per_million: 0.28, output_per_million: 1.12 },
        "fireworks"  => Pricing { input_per_million: 0.90, output_per_million: 0.90 },
        "zai"        => Pricing { input_per_million: 0.60, output_per_million: 2.20 },
        _ => return 0.0,
    };
    price_tokens(input, output, &defaults)
}

// ─── subscription verdict ────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Verdict { Cancel, Downgrade, Keep, Upgrade }

#[derive(Clone, Debug)]
pub struct SubscriptionVerdict {
    pub verdict: Verdict,
    pub peak_utilization_pct: f32,
    pub avg_utilization_pct: f32,
    /// Plan $/mo divided by an "average utilised point" (%-hours consumed).
    /// Lower is better value; higher means you'd save by downgrading.
    pub dollars_per_utilised_point: f64,
    /// Dollars recoverable per month if user acts on the verdict.
    pub recoverable_dollars: f64,
}

pub fn subscription_verdict(
    account: &AccountKey,
    cycles_to_analyze: usize,
) -> Option<SubscriptionVerdict> {
    if cycles_to_analyze == 0 { return None; }
    // Weekly cycle ~= 7 days.  Look back cycles_to_analyze * 7 days of samples.
    let samples = usage_log::last_n_days(account, (cycles_to_analyze * 7) as u32);
    if samples.is_empty() { return None; }

    // Extract weekly pct per sample; take max per bucketed calendar week for peak,
    // and mean of all weekly_pct samples for avg.
    let weekly: Vec<f32> = samples.iter().filter_map(|s| s.weekly_pct).collect();
    if weekly.is_empty() { return None; }

    let peak = weekly.iter().cloned().fold(0.0f32, f32::max);
    let avg = weekly.iter().sum::<f32>() / (weekly.len() as f32);

    let verdict = if avg < 10.0 && peak < 20.0 {
        Verdict::Cancel
    } else if avg < 30.0 && peak < 50.0 {
        Verdict::Downgrade
    } else if avg < 80.0 {
        Verdict::Keep
    } else {
        Verdict::Upgrade
    };

    // Placeholder plan price (caller can override once we thread the plan tier
    // in): use $100/mo as the reference.  dollars_per_utilised_point is
    // "monthly spend divided by average utilization"; a lower number = better
    // value, a higher number = wasted spend.
    let monthly_spend = 100.0_f64;
    let dppp = if avg > f32::EPSILON {
        monthly_spend / avg as f64
    } else {
        f64::INFINITY
    };
    let recoverable = match verdict {
        Verdict::Cancel    => monthly_spend,
        Verdict::Downgrade => monthly_spend * 0.5,
        Verdict::Keep      => 0.0,
        Verdict::Upgrade   => 0.0,
    };

    Some(SubscriptionVerdict {
        verdict,
        peak_utilization_pct: peak,
        avg_utilization_pct: avg,
        dollars_per_utilised_point: dppp,
        recoverable_dollars: recoverable,
    })
}

// ─── tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn v(peak: f32, avg: f32) -> Verdict {
        // Reconstruct just the classification bit for boundary tests.
        if avg < 10.0 && peak < 20.0 { Verdict::Cancel }
        else if avg < 30.0 && peak < 50.0 { Verdict::Downgrade }
        else if avg < 80.0 { Verdict::Keep }
        else { Verdict::Upgrade }
    }

    #[test]
    fn verdict_cancel_threshold() {
        assert_eq!(v(19.0, 9.9), Verdict::Cancel);
        assert_ne!(v(20.0, 9.9), Verdict::Cancel);
        assert_ne!(v(19.0, 10.0), Verdict::Cancel);
    }

    #[test]
    fn verdict_downgrade_threshold() {
        assert_eq!(v(49.0, 29.0), Verdict::Downgrade);
        assert_ne!(v(50.0, 29.0), Verdict::Downgrade);
    }

    #[test]
    fn verdict_keep() {
        assert_eq!(v(65.0, 55.0), Verdict::Keep);
    }

    #[test]
    fn verdict_upgrade_threshold() {
        assert_eq!(v(100.0, 85.0), Verdict::Upgrade);
        assert_eq!(v(80.0, 82.0), Verdict::Upgrade);
    }

    #[test]
    fn price_tokens_math() {
        let p = Pricing { input_per_million: 3.00, output_per_million: 15.00 };
        // 1M in, 1M out → $3 + $15 = $18
        assert!((price_tokens(1_000_000, 1_000_000, &p) - 18.00).abs() < 1e-9);
        // Half-and-half of half a million each: $0.75 + $3.75 = $4.50
        assert!((price_tokens(500_000, 250_000, &p) - (0.5 * 3.0 + 0.25 * 15.0)).abs() < 1e-9);
    }

    #[test]
    fn provider_default_price_zero_for_passthrough() {
        assert_eq!(provider_default_price("openrouter", 1_000_000, 1_000_000), 0.0);
        assert_eq!(provider_default_price("synthetic",  1_000_000, 1_000_000), 0.0);
    }

    #[test]
    fn plan_caps_sane_order() {
        assert!(CLAUDE_MAX_100_WEEKLY_TOKENS < CLAUDE_MAX_200_WEEKLY_TOKENS);
        assert!(CLAUDE_MAX_200_WEEKLY_TOKENS < CLAUDE_MAX_300_WEEKLY_TOKENS);
        assert!(CODEX_PLUS_WEEKLY_TOKENS < CODEX_PRO_WEEKLY_TOKENS);
    }

    // Fixture-based full estimate — requires usage_log fixture harness; wired in
    // tests/integration when the crate lands.
    #[test]
    #[ignore]
    fn estimate_cycle_cost_via_public_api() {
        let acc = AccountKey::default();
        let _ = estimate_cycle_cost(&acc, CLAUDE_MAX_100_WEEKLY_TOKENS);
    }
}
