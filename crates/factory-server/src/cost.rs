/// Formats a cost in USD with two decimals, such as `$1.23`.
#[must_use]
pub fn format_cost(usd: f64) -> String {
    format!("${usd:.2}")
}
