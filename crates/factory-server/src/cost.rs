/// Formats a cost in USD with two decimals, such as `$1.23`.
#[must_use]
pub fn format_cost(usd: f64) -> String {
    format!("${usd:.2}")
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
