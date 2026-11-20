use factory_core::{PLAN_USAGE_STALE_AFTER_SECS, PlanUsage, PlanUsageSnapshot};

/// A `?skip_spend=1` body like the one the spike documented: the three
/// windows, each with a `status`, and a `model_scoped` array that carries
/// Fable. The parser keeps the fields the tile needs and ignores the rest.
const SAMPLE: &str = r#"{
    "five_hour": { "utilization": 0.42, "resets_at": 1763660000, "status": "allowed" },
    "seven_day": { "utilization": 0.73, "resets_at": 1764200000, "status": "allowed_warning" },
    "seven_day_overage_included": { "utilization": 0.10, "resets_at": 1764200000 },
    "model_scoped": [
        { "display_name": "Fable", "utilization": 0.55, "resets_at": 1764200000 }
    ]
}"#;

#[test]
fn parses_a_sample_response() {
    let usage = PlanUsage::parse(SAMPLE).expect("sample parses");

    assert_eq!(usage.five_hour.utilization, 0.42);
    assert_eq!(usage.five_hour.resets_at, 1_763_660_000);
    assert_eq!(usage.seven_day.utilization, 0.73);
    assert_eq!(usage.seven_day.resets_at, 1_764_200_000);

    let fable = usage.model_window("Fable").expect("Fable window present");
    assert_eq!(fable.window.utilization, 0.55);
    assert_eq!(fable.window.resets_at, 1_764_200_000);
}

#[test]
fn percent_used_scales_the_fraction() {
    let usage = PlanUsage::parse(SAMPLE).expect("sample parses");
    assert_eq!(usage.five_hour.percent_used(), 42.0);
}

#[test]
fn a_missing_model_scoped_array_leaves_no_model_windows() {
    let body = r#"{
        "five_hour": { "utilization": 0.1, "resets_at": 1763660000 },
        "seven_day": { "utilization": 0.2, "resets_at": 1764200000 }
    }"#;

    let usage = PlanUsage::parse(body).expect("body without model_scoped parses");

    assert_eq!(usage.model_scoped.len(), 0);
    assert!(usage.model_window("Fable").is_none());
}

#[test]
fn malformed_json_is_an_error() {
    assert!(PlanUsage::parse("not json").is_err());
}

#[test]
fn a_missing_window_is_an_error() {
    let body = r#"{ "five_hour": { "utilization": 0.1, "resets_at": 1763660000 } }"#;
    assert!(PlanUsage::parse(body).is_err());
}

fn snapshot(fetched_at: i64) -> PlanUsageSnapshot {
    PlanUsageSnapshot {
        usage: PlanUsage::parse(SAMPLE).expect("sample parses"),
        fetched_at,
    }
}

#[test]
fn a_recent_snapshot_is_fresh() {
    let snap = snapshot(1_000);
    let now = 1_000 + PLAN_USAGE_STALE_AFTER_SECS - 1;

    assert!(snap.is_fresh(now));
    assert!(!snap.is_stale(now));
}

#[test]
fn a_snapshot_past_the_cutoff_is_stale() {
    let snap = snapshot(1_000);
    let now = 1_000 + PLAN_USAGE_STALE_AFTER_SECS;

    assert!(snap.is_stale(now));
    assert!(!snap.is_fresh(now));
}
