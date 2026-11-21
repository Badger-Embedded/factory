use factory_core::{PlanUsage, UsageWindow};
use factory_server::PlanUsageCache;

fn window(utilization: f64) -> UsageWindow {
    UsageWindow {
        utilization,
        resets_at: 4_102_444_800,
    }
}

#[test]
fn an_empty_cache_has_no_snapshot() {
    assert!(PlanUsageCache::new().snapshot().is_none());
}

#[test]
fn a_report_fills_the_cache_with_its_time() {
    let cache = PlanUsageCache::new();
    let usage = PlanUsage {
        five_hour: Some(window(0.4)),
        seven_day: Some(window(0.7)),
        fable: None,
    };

    cache.record(usage.clone(), 1_000);

    let snapshot = cache.snapshot().expect("a snapshot after a report");
    assert_eq!(snapshot.usage, usage);
    assert_eq!(snapshot.reported_at, 1_000);
}

#[test]
fn a_later_report_keeps_the_windows_it_leaves_out() {
    let cache = PlanUsageCache::new();
    cache.record(
        PlanUsage {
            five_hour: Some(window(0.4)),
            seven_day: Some(window(0.7)),
            fable: Some(window(0.1)),
        },
        1_000,
    );

    cache.record(
        PlanUsage {
            five_hour: Some(window(0.5)),
            ..PlanUsage::default()
        },
        2_000,
    );

    let snapshot = cache.snapshot().unwrap();
    assert_eq!(snapshot.usage.five_hour, Some(window(0.5)));
    assert_eq!(snapshot.usage.seven_day, Some(window(0.7)));
    assert_eq!(snapshot.usage.fable, Some(window(0.1)));
    assert_eq!(snapshot.reported_at, 2_000);
}
