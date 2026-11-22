//! Tests for the Copilot device login on its settings page, driven by a fake
//! login container. No podman runs: the fake prints Copilot's prompt with ANSI
//! color, then writes `config.json` or exits in failure.

#[path = "support/copilot_harness.rs"]
mod harness;

use std::time::Duration;

use axum::http::StatusCode;
use harness::{Outcome, Script, config_json, harness, harness_fast_timeout};

const TOKEN: &str = "gho_abcdefghijklmnopqrstuvwxyz0123456789";

fn prompt() -> Vec<String> {
    vec![
        "To authenticate, visit \u{1b}[94mhttps://github.com/login/device\u{1b}[0m and enter code \u{1b}[1m1234-5678\u{1b}[0m"
            .to_owned(),
    ]
}

#[tokio::test]
async fn the_url_and_code_show_without_color_codes_then_the_token_is_stored() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(config_json(TOKEN)),
    });

    let start = harness.post("/settings/agents/copilot/login", "").await;
    assert_eq!(start.status, StatusCode::SEE_OTHER);
    assert_eq!(start.headers["location"], "/settings/agents/copilot");

    let waiting = harness.poll_fragment("1234-5678").await;
    assert!(waiting.contains("https://github.com/login/device"));
    assert!(!waiting.contains("94m"));
    assert_eq!(harness.spawner.spawned(), ["factory-copilot-login"]);

    harness.spawner.finish();
    harness.poll_fragment("Signed in").await;
    assert_eq!(
        harness.secrets.copilot_token().unwrap().as_deref(),
        Some(TOKEN)
    );
    assert!(harness.secrets.codex_auth().unwrap().is_none());
}

#[tokio::test]
async fn signing_in_reloads_the_page_so_the_stored_status_updates() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(config_json(TOKEN)),
    });
    assert!(
        harness
            .get("/settings/agents/copilot")
            .await
            .body
            .contains("No token is stored.")
    );

    let start = harness.post("/settings/agents/copilot/login", "").await;
    assert_eq!(start.status, StatusCode::SEE_OTHER);
    let waiting = harness.get("/settings/agents/copilot/login/fragment").await;
    assert!(waiting.headers.get("hx-refresh").is_none());

    harness.spawner.finish();
    harness.poll_fragment("Signed in").await;
    let done = harness.get("/settings/agents/copilot/login/fragment").await;
    assert_eq!(done.headers.get("hx-refresh").unwrap(), "true");

    let page = harness.get("/settings/agents/copilot").await;
    assert!(page.body.contains("A token is stored."));
    assert!(!page.body.contains("No token is stored."));
}

#[tokio::test]
async fn the_login_home_is_under_the_data_directory_and_deleted_at_the_end() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(config_json(TOKEN)),
    });

    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;

    let homes = harness.spawner.homes();
    assert_eq!(homes.len(), 1);
    assert!(homes[0].starts_with(harness.data.path().join("copilot-login")));
    let settings = std::fs::read_to_string(homes[0].join("settings.json")).unwrap();
    assert!(settings.contains("\"storeTokenPlaintext\": true"));

    harness.spawner.finish();
    harness.poll_fragment("Signed in").await;
    assert!(!homes[0].exists());
}

#[tokio::test]
async fn a_failed_login_shows_a_scrubbed_reason_and_stores_nothing() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: vec![format!("Error: bad credentials for {TOKEN}")],
        outcome: Outcome::Failure,
    });

    harness.post("/settings/agents/copilot/login", "").await;
    harness.spawner.finish();

    let failed = harness.poll_fragment("did not finish").await;
    assert!(failed.contains("[redacted]"));
    assert!(!failed.contains(TOKEN));
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn a_config_with_no_token_fails_the_login() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(r#"{"authTokens":{}}"#.to_owned()),
    });

    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;
    harness.spawner.finish();

    harness.poll_fragment("did not finish").await;
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn a_token_the_cli_would_refuse_is_not_stored() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(config_json("ghp_abcdefghijklmnopqrstuvwxyz0123456789")),
    });

    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;
    harness.spawner.finish();

    harness.poll_fragment("did not finish").await;
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn cancel_removes_the_container_and_stores_nothing() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(config_json(TOKEN)),
    });

    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;

    let reply = harness
        .post("/settings/agents/copilot/login/cancel", "")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers["location"], "/settings/agents/copilot");
    for _ in 0..200 {
        if !harness.spawner.killed().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(harness.spawner.killed(), ["factory-copilot-login"]);
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn only_one_login_runs_at_a_time() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(config_json(TOKEN)),
    });

    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;
    harness.post("/settings/agents/copilot/login", "").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.spawner.spawned().len(), 1);
}

#[tokio::test]
async fn a_login_that_never_finishes_times_out_with_a_reason() {
    let harness = harness_fast_timeout();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(config_json(TOKEN)),
    });

    harness.post("/settings/agents/copilot/login", "").await;
    let failed = harness.poll_fragment("did not finish").await;
    assert!(failed.contains("fifteen minutes"));
    assert!(harness.secrets.copilot_token().unwrap().is_none());
    assert_eq!(harness.spawner.killed(), ["factory-copilot-login"]);
}
