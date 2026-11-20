use factory_core::PLAN_USAGE_STALE_AFTER_SECS;
use factory_server::{PlanUsageCache, UsageSource, refresh_once};

/// A `?skip_spend=1` body like the one the spike documented, with Fable in the
/// `model_scoped` array.
const SAMPLE: &str = r#"{
    "five_hour": { "utilization": 0.42, "resets_at": 1763660000, "status": "allowed" },
    "seven_day": { "utilization": 0.73, "resets_at": 1764200000 },
    "model_scoped": [
        { "display_name": "Fable", "utilization": 0.55, "resets_at": 1764200000 }
    ]
}"#;

/// A source that returns a fixed result, so a test drives the cache without a
/// network call.
struct Fake(Result<String, String>);

impl UsageSource for Fake {
    fn fetch(&self) -> impl std::future::Future<Output = Result<String, String>> {
        std::future::ready(self.0.clone())
    }
}

fn ok(body: &str) -> Fake {
    Fake(Ok(body.to_owned()))
}

fn err() -> Fake {
    Fake(Err(
        "curl failed: The requested URL returned error: 429".into()
    ))
}

#[tokio::test]
async fn a_successful_fetch_fills_the_cache() {
    let cache = PlanUsageCache::new();

    refresh_once(&ok(SAMPLE), &cache).await;

    let usage = cache
        .visible(now())
        .expect("the tile shows after a success");
    assert_eq!(usage.five_hour.utilization, 0.42);
    assert_eq!(usage.seven_day.utilization, 0.73);
    assert_eq!(
        usage
            .model_window("Fable")
            .expect("Fable window")
            .window
            .utilization,
        0.55
    );
}

#[tokio::test]
async fn a_failed_fetch_hides_the_tile() {
    let cache = PlanUsageCache::new();

    refresh_once(&err(), &cache).await;

    assert!(cache.visible(now()).is_none());
}

#[tokio::test]
async fn an_unparseable_body_hides_the_tile() {
    let cache = PlanUsageCache::new();

    refresh_once(&ok("not json"), &cache).await;

    assert!(cache.visible(now()).is_none());
}

#[tokio::test]
async fn an_error_after_a_success_hides_the_tile() {
    let cache = PlanUsageCache::new();

    refresh_once(&ok(SAMPLE), &cache).await;
    refresh_once(&err(), &cache).await;

    assert!(cache.visible(now()).is_none());
}

#[tokio::test]
async fn a_success_after_an_error_shows_the_tile_again() {
    let cache = PlanUsageCache::new();

    refresh_once(&err(), &cache).await;
    refresh_once(&ok(SAMPLE), &cache).await;

    assert!(cache.visible(now()).is_some());
}

#[tokio::test]
async fn a_stale_success_hides_the_tile() {
    let cache = PlanUsageCache::new();

    refresh_once(&ok(SAMPLE), &cache).await;
    let later = now() + PLAN_USAGE_STALE_AFTER_SECS + 1;

    assert!(cache.visible(later).is_none());
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}
