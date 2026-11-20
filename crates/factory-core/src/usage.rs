//! Claude plan usage: the subscription limits the board tile shows.
//!
//! The server fetches these from `GET /api/oauth/usage?skip_spend=1` with the
//! factory's `setup-token`, then caches a [`PlanUsageSnapshot`]. The parsing
//! and the staleness rule live here so they can be tested without a network
//! call. See `docs/design.md`, "Claude plan usage source", for the endpoint
//! and the fields.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How long a fetched snapshot stays fresh. A snapshot older than this is
/// stale and the tile is hidden, so a run of failed fetches blanks the tile
/// instead of showing a stale number. The server's refresh interval must be
/// shorter than this, so one missed refresh does not hide the tile.
pub const PLAN_USAGE_STALE_AFTER_SECS: i64 = 30 * 60;

/// One window the plan reports, such as the 5-hour or the weekly limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// The fraction of the window used, as the source reports it. `0.0` is an
    /// empty window and `1.0` is a full one.
    pub utilization: f64,
    /// The Unix timestamp, in seconds, when the window resets.
    pub resets_at: i64,
}

impl UsageWindow {
    /// The fraction used as a percentage, `0.0` to `100.0`.
    #[must_use]
    pub fn percent_used(&self) -> f64 {
        self.utilization * 100.0
    }
}

/// A per-model weekly window from the response's `model_scoped` array. Fable
/// usage arrives here, named by `display_name`, not as a field of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelWindow {
    /// The model name the server supplies, for example `Fable`.
    pub display_name: String,
    #[serde(flatten)]
    pub window: UsageWindow,
}

/// The plan usage the tile shows: the 5-hour window, the weekly window, and
/// the per-model windows the Fable usage comes through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanUsage {
    pub five_hour: UsageWindow,
    pub seven_day: UsageWindow,
    /// The per-model windows, empty when the server reports none.
    #[serde(default)]
    pub model_scoped: Vec<ModelWindow>,
}

impl PlanUsage {
    /// Parses a `GET /api/oauth/usage?skip_spend=1` response body.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] when the body is not JSON or lacks the
    /// `five_hour` or `seven_day` window.
    pub fn parse(body: &str) -> Result<PlanUsage, ParseError> {
        serde_json::from_str(body).map_err(|source| ParseError {
            message: source.to_string(),
        })
    }

    /// The first per-model window whose name matches `display_name`, if any.
    #[must_use]
    pub fn model_window(&self, display_name: &str) -> Option<&ModelWindow> {
        self.model_scoped
            .iter()
            .find(|window| window.display_name == display_name)
    }
}

/// A parsed snapshot together with the time it was fetched, so the server can
/// cache it and the tile can check whether it is still fresh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanUsageSnapshot {
    pub usage: PlanUsage,
    /// The Unix timestamp, in seconds, when the fetch succeeded.
    pub fetched_at: i64,
}

impl PlanUsageSnapshot {
    /// Whether the snapshot is within [`PLAN_USAGE_STALE_AFTER_SECS`] of
    /// `now`, both Unix timestamps in seconds. A snapshot from the future
    /// (negative age) counts as fresh.
    #[must_use]
    pub fn is_fresh(&self, now: i64) -> bool {
        now - self.fetched_at < PLAN_USAGE_STALE_AFTER_SECS
    }

    /// The negation of [`is_fresh`](Self::is_fresh). The tile is hidden when
    /// this is true.
    #[must_use]
    pub fn is_stale(&self, now: i64) -> bool {
        !self.is_fresh(now)
    }
}

/// A response body that could not be parsed into a [`PlanUsage`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not parse plan usage: {}", self.message)
    }
}

impl std::error::Error for ParseError {}
