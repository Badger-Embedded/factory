//! The Claude plan usage the board tile shows.
//!
//! Worker runs report plan usage through their `rate_limit_event` lines, and
//! the podman executor writes each report into a [`PlanUsageCache`] shared
//! with the dashboard. See `docs/design.md`, "Claude plan usage source".

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use factory_core::{PlanUsage, PlanUsageSnapshot};

/// The plan usage the factory last saw, shared between the runs that write it
/// and the dashboard that reads it.
#[derive(Default)]
pub struct PlanUsageCache {
    state: Mutex<Option<PlanUsageSnapshot>>,
}

impl PlanUsageCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a report a run made at `reported_at`, a Unix timestamp in
    /// seconds. Each window the report carries replaces the cached one, and the
    /// windows it leaves out stay as they were.
    pub fn record(&self, usage: PlanUsage, reported_at: i64) {
        let mut state = self.lock();
        match state.as_mut() {
            Some(snapshot) => {
                snapshot.usage.merge(usage);
                snapshot.reported_at = reported_at;
            }
            None => {
                *state = Some(PlanUsageSnapshot { usage, reported_at });
            }
        }
    }

    /// Records a report a run made now.
    pub fn record_now(&self, usage: PlanUsage) {
        self.record(usage, now_secs());
    }

    /// The last snapshot, or `None` before any run has reported.
    #[must_use]
    pub fn snapshot(&self) -> Option<PlanUsageSnapshot> {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Option<PlanUsageSnapshot>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The current Unix time in seconds, or 0 before the epoch.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}
