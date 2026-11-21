use factory_runner::TokenUsage;

use crate::store::CodexPrice;

/// Formats a cost in USD with two decimals, such as `$1.23`.
#[must_use]
pub fn format_cost(usd: f64) -> String {
    format!("${usd:.2}")
}

/// The estimated USD cost of a Codex run from its summed token usage and the
/// price for its model. `cached_input_tokens` is the part of `input_tokens` the
/// prompt cache served, so the uncached input is the rest, billed at the input
/// price, while the cached part is billed at the cheaper cached price.
/// `reasoning_output_tokens` is already part of `output_tokens`, so the output
/// is billed once at the output price. Each price is USD per million tokens.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "token counts stay far below 2^52, where f64 is exact"
)]
pub fn codex_cost(usage: TokenUsage, price: &CodexPrice) -> f64 {
    let uncached_input = usage.input_tokens.saturating_sub(usage.cached_input_tokens);
    let per_mtok = |tokens: u64, usd_per_mtok: f64| tokens as f64 / 1_000_000.0 * usd_per_mtok;
    per_mtok(uncached_input, price.input_usd_per_mtok)
        + per_mtok(usage.cached_input_tokens, price.cached_input_usd_per_mtok)
        + per_mtok(usage.output_tokens, price.output_usd_per_mtok)
}

/// The share of created jobs that merged, from 0.0 to 1.0. It is 0.0 when no
/// job was created, so the merge-rate gauge draws empty instead of NaN.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "job counts stay far below 2^52")]
pub fn merge_fraction(created: i64, merged: i64) -> f64 {
    if created <= 0 {
        return 0.0;
    }
    (merged as f64 / created as f64).clamp(0.0, 1.0)
}
