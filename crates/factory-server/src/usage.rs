//! Fetch, cache and refresh the Claude plan usage the board tile shows.
//!
//! A background task calls the usage endpoint on a fixed interval and writes
//! the result into a [`PlanUsageCache`] shared with the dashboard, so a page
//! load reads the cache and never calls the endpoint itself. The parsing and
//! the staleness rule live in `factory_core`; see `docs/design.md`, "Claude
//! plan usage source", for the endpoint and the fields.

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use factory_core::{PlanUsage, PlanUsageSnapshot};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// How often the background task fetches plan usage. It is shorter than
/// `PLAN_USAGE_STALE_AFTER_SECS`, so one failed fetch does not make the cached
/// snapshot stale and hide the tile.
pub const PLAN_USAGE_REFRESH: Duration = Duration::from_mins(5);

/// The usage endpoint the interactive `/usage` panel calls, with the spend
/// figures left out.
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage?skip_spend=1";

/// The beta header the endpoint requires for an OAuth token.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// A source of the raw usage response body. The real source shells out to
/// `curl`; tests pass a fake, so they need no network.
pub trait UsageSource {
    /// Fetches the usage endpoint and returns the response body, or an error
    /// message with no secret in it.
    fn fetch(&self) -> impl Future<Output = Result<String, String>> + Send;
}

/// Fetches plan usage by running `curl` with the factory's Claude token.
pub struct CurlUsageSource {
    /// The `curl` binary.
    pub curl: PathBuf,
    /// The factory's Claude subscription token, from the `claude-token`
    /// secret the workers already use.
    pub token: String,
}

impl UsageSource for CurlUsageSource {
    async fn fetch(&self) -> Result<String, String> {
        // curl reads the URL and the auth header from its stdin config, so the
        // token never appears in the process arguments.
        let config = format!(
            "url = \"{USAGE_URL}\"\n\
             header = \"Authorization: Bearer {}\"\n\
             header = \"anthropic-beta: {OAUTH_BETA}\"\n",
            self.token
        );
        let mut child = Command::new(&self.curl)
            .arg("--silent")
            .arg("--show-error")
            .arg("--fail")
            .arg("--max-time")
            .arg("30")
            .arg("--config")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("could not start {}: {err}", self.curl.display()))?;
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(config.as_bytes())
                .await
                .map_err(|err| format!("writing the curl config: {err}"))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|err| format!("waiting for curl: {err}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "curl failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// The last plan usage fetch, shared between the background task that writes
/// it and the dashboard that reads it.
#[derive(Default)]
pub struct PlanUsageCache {
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    /// The last fetch that parsed, or `None` before the first one.
    last_success: Option<PlanUsageSnapshot>,
    /// Whether the most recent fetch failed to reach the endpoint or parse.
    errored: bool,
}

impl PlanUsageCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a parsed response fetched at `fetched_at`, a Unix timestamp in
    /// seconds, and clears the error flag.
    pub fn record_success(&self, usage: PlanUsage, fetched_at: i64) {
        let mut state = self.lock();
        state.last_success = Some(PlanUsageSnapshot { usage, fetched_at });
        state.errored = false;
    }

    /// Marks the most recent fetch as failed. The last success is kept so a
    /// later success can replace it, but the tile stays hidden until then.
    pub fn record_error(&self) {
        self.lock().errored = true;
    }

    /// The usage to show at `now`, a Unix timestamp in seconds, or `None` when
    /// the tile must be hidden: the last fetch errored, no fetch has ever
    /// parsed, or the last success is stale.
    #[must_use]
    pub fn visible(&self, now: i64) -> Option<PlanUsage> {
        let state = self.lock();
        if state.errored {
            return None;
        }
        let snapshot = state.last_success.as_ref()?;
        snapshot.is_fresh(now).then(|| snapshot.usage.clone())
    }

    fn lock(&self) -> MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Fetches plan usage from `source` once and writes the result into `cache`. A
/// failed fetch or an unparseable body is logged without the token and marks
/// the cache as errored.
pub async fn refresh_once<S: UsageSource>(source: &S, cache: &PlanUsageCache) {
    match source.fetch().await {
        Ok(body) => match PlanUsage::parse(&body) {
            Ok(usage) => cache.record_success(usage, now_secs()),
            Err(err) => {
                tracing::warn!("plan usage did not parse: {err}");
                cache.record_error();
            }
        },
        Err(err) => {
            tracing::warn!("plan usage fetch failed: {err}");
            cache.record_error();
        }
    }
}

/// Refreshes `cache` from `source` now and every [`PLAN_USAGE_REFRESH`]. The
/// first tick fires at once, so the tile can appear soon after startup.
pub async fn refresh_forever<S: UsageSource>(source: S, cache: std::sync::Arc<PlanUsageCache>) {
    let mut ticker = tokio::time::interval(PLAN_USAGE_REFRESH);
    loop {
        ticker.tick().await;
        refresh_once(&source, &cache).await;
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
