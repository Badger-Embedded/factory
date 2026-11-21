//! Claude plan usage: the subscription limits the board tile shows.
//!
//! Worker runs report the limits. Claude Code writes a `rate_limit_event` line
//! to its stream-json output with the plan windows it read from the API's
//! response headers, and the factory parses the line's `rate_limit_info`
//! object here. See `docs/design.md`, "Claude plan usage source", for why the
//! factory does not call the usage endpoint.

use std::fmt;

use serde::Deserialize;
use serde_json::Value;

/// One window the plan reports, such as the 5-hour or the weekly limit.
#[derive(Debug, Clone, PartialEq)]
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

    /// Whether the window reset at or before `now`, a Unix timestamp in
    /// seconds. Its `utilization` then describes the window before the reset.
    #[must_use]
    pub fn has_reset(&self, now: i64) -> bool {
        self.resets_at <= now
    }
}

/// The plan usage the tile shows. Each window is `None` until a run reports
/// it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanUsage {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    /// The weekly Fable window, which Claude Code names
    /// `seven_day_overage_included` and labels "Fable limit".
    pub fable: Option<UsageWindow>,
}

/// The `rate_limit_info` object. `unifiedWindows` carries every window the
/// API reported. Claude Code versions that predate it send only the window
/// named by `rateLimitType`, with its numbers at the top level.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitInfo {
    #[serde(default)]
    unified_windows: Option<UnifiedWindows>,
    rate_limit_type: Option<String>,
    utilization: Option<f64>,
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
struct UnifiedWindows {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
    seven_day_overage_included: Option<RawWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWindow {
    utilization: f64,
    resets_at: i64,
}

impl From<RawWindow> for UsageWindow {
    fn from(raw: RawWindow) -> Self {
        UsageWindow {
            utilization: raw.utilization,
            resets_at: raw.resets_at,
        }
    }
}

impl From<UnifiedWindows> for PlanUsage {
    fn from(windows: UnifiedWindows) -> Self {
        PlanUsage {
            five_hour: windows.five_hour.map(UsageWindow::from),
            seven_day: windows.seven_day.map(UsageWindow::from),
            fable: windows.seven_day_overage_included.map(UsageWindow::from),
        }
    }
}

/// The one window an event without `unifiedWindows` reports, named by its
/// `rateLimitType`.
fn single_window(
    kind: Option<String>,
    utilization: Option<f64>,
    resets_at: Option<i64>,
) -> PlanUsage {
    let mut usage = PlanUsage::default();
    let (Some(kind), Some(utilization), Some(resets_at)) = (kind, utilization, resets_at) else {
        return usage;
    };
    let window = Some(UsageWindow {
        utilization,
        resets_at,
    });
    match kind.as_str() {
        "five_hour" => usage.five_hour = window,
        "seven_day" => usage.seven_day = window,
        "seven_day_overage_included" => usage.fable = window,
        _ => {}
    }
    usage
}

impl PlanUsage {
    /// Parses the `rate_limit_info` object of a `rate_limit_event` line.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] when the object does not have the expected shape
    /// or reports none of the windows the tile shows.
    pub fn from_rate_limit_info(info: Value) -> Result<PlanUsage, ParseError> {
        let info: RateLimitInfo = serde_json::from_value(info).map_err(|source| ParseError {
            message: source.to_string(),
        })?;
        let usage = match info.unified_windows {
            Some(windows) => PlanUsage::from(windows),
            None => single_window(info.rate_limit_type, info.utilization, info.resets_at),
        };
        if usage.is_empty() {
            return Err(ParseError {
                message: "no 5-hour, weekly or Fable window".into(),
            });
        }
        Ok(usage)
    }

    /// Parses a `rate_limit_info` object from its JSON text.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] when the text is not JSON or
    /// [`from_rate_limit_info`](Self::from_rate_limit_info) rejects it.
    pub fn parse(body: &str) -> Result<PlanUsage, ParseError> {
        let value = serde_json::from_str(body).map_err(|source: serde_json::Error| ParseError {
            message: source.to_string(),
        })?;
        Self::from_rate_limit_info(value)
    }

    /// Whether no window is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none() && self.fable.is_none()
    }

    /// Takes each window `newer` reports and keeps the rest, so a report that
    /// names only the 5-hour window leaves the weekly one in place.
    pub fn merge(&mut self, newer: PlanUsage) {
        if newer.five_hour.is_some() {
            self.five_hour = newer.five_hour;
        }
        if newer.seven_day.is_some() {
            self.seven_day = newer.seven_day;
        }
        if newer.fable.is_some() {
            self.fable = newer.fable;
        }
    }
}

/// The plan usage the factory last saw and when a run reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanUsageSnapshot {
    pub usage: PlanUsage,
    /// The Unix timestamp, in seconds, of the latest report.
    pub reported_at: i64,
}

/// A `rate_limit_info` object that could not be parsed into a [`PlanUsage`].
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
