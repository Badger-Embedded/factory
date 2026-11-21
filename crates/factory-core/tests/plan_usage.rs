use factory_core::{PlanUsage, UsageWindow};

/// The `rate_limit_info` object Claude Code 2.1.330 wrote on moon on
/// 2026-11-21, with a Fable window added.
const SAMPLE: &str = r#"{
    "status": "allowed",
    "resetsAt": 1795234800,
    "rateLimitType": "five_hour",
    "overageStatus": "rejected",
    "overageDisabledReason": "org_level_disabled",
    "isUsingOverage": false,
    "unifiedWindows": {
        "five_hour": { "utilization": 0.77, "resetsAt": 1795234800, "status": "allowed" },
        "seven_day": { "utilization": 0.33, "resetsAt": 1795676400, "status": "allowed" },
        "seven_day_overage_included": { "utilization": 0.5, "resetsAt": 1795676400, "status": "allowed" }
    }
}"#;

fn window(utilization: f64, resets_at: i64) -> UsageWindow {
    UsageWindow {
        utilization,
        resets_at,
    }
}

#[test]
fn parses_the_unified_windows() {
    let usage = PlanUsage::parse(SAMPLE).expect("sample parses");

    assert_eq!(usage.five_hour, Some(window(0.77, 1_795_234_800)));
    assert_eq!(usage.seven_day, Some(window(0.33, 1_795_676_400)));
    assert_eq!(usage.fable, Some(window(0.5, 1_795_676_400)));
}

#[test]
fn a_window_the_event_leaves_out_is_none() {
    let body = r#"{ "unifiedWindows": {
        "five_hour": { "utilization": 0.1, "resetsAt": 1795234800 },
        "seven_day": { "utilization": 0.2, "resetsAt": 1795676400 }
    } }"#;

    let usage = PlanUsage::parse(body).expect("body without Fable parses");

    assert!(usage.five_hour.is_some());
    assert!(usage.fable.is_none());
}

#[test]
fn an_event_without_unified_windows_reports_its_one_window() {
    let body = r#"{ "status": "allowed_warning", "rateLimitType": "seven_day",
        "utilization": 0.9, "resetsAt": 1795676400 }"#;

    let usage = PlanUsage::parse(body).expect("legacy shape parses");

    assert_eq!(usage.seven_day, Some(window(0.9, 1_795_676_400)));
    assert!(usage.five_hour.is_none());
}

#[test]
fn an_event_with_no_window_is_an_error() {
    assert!(PlanUsage::parse(r#"{ "status": "allowed", "resetsAt": 1795234800 }"#).is_err());
    assert!(PlanUsage::parse(r#"{ "unifiedWindows": {} }"#).is_err());
    assert!(PlanUsage::parse("not json").is_err());
}

#[test]
fn percent_used_scales_the_fraction() {
    let usage = PlanUsage::parse(SAMPLE).expect("sample parses");
    assert_eq!(usage.seven_day.unwrap().percent_used(), 33.0);
}

#[test]
fn a_window_has_reset_once_its_reset_time_passes() {
    let window = window(0.5, 1_000);
    assert!(!window.has_reset(999));
    assert!(window.has_reset(1_000));
}

#[test]
fn merge_keeps_the_windows_a_newer_report_leaves_out() {
    let mut usage = PlanUsage::parse(SAMPLE).expect("sample parses");
    let newer = PlanUsage {
        five_hour: Some(window(0.8, 1_795_300_000)),
        ..PlanUsage::default()
    };

    usage.merge(newer);

    assert_eq!(usage.five_hour, Some(window(0.8, 1_795_300_000)));
    assert_eq!(usage.seven_day, Some(window(0.33, 1_795_676_400)));
    assert_eq!(usage.fable, Some(window(0.5, 1_795_676_400)));
}
