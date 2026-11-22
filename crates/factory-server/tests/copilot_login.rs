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
    let location = start.headers["location"].to_str().unwrap().to_owned();
    let polling = harness.get(&location).await;
    assert_eq!(polling.status, StatusCode::OK);
    assert!(
        polling
            .body
            .contains("/settings/agents/copilot/login/fragment")
    );
    assert!(polling.body.contains("hx-trigger"));
    let waiting = harness.get("/settings/agents/copilot/login/fragment").await;
    assert!(waiting.headers.get("hx-refresh").is_none());

    harness.spawner.finish();
    harness.poll_fragment("Signed in").await;
    let done = harness.get("/settings/agents/copilot/login/fragment").await;
    assert_eq!(done.headers.get("hx-refresh").unwrap(), "true");

    let page = harness.get(&location).await;
    assert!(page.body.contains("A token is stored."));
    assert!(!page.body.contains("No token is stored."));
    assert!(!page.body.contains("hx-trigger"));
    assert_eq!(harness.spawner.spawned().len(), 1);
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

    let failed = harness.poll_fragment("could not store a token").await;
    assert!(failed.contains("held no token"), "{failed}");
    assert!(failed.contains("Keys under authTokens: none"), "{failed}");
    assert!(failed.contains("The directory holds:"), "{failed}");
    assert!(failed.contains("settings.json"), "{failed}");
    assert!(!failed.contains("did not finish"));
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

    let failed = harness.poll_fragment("could not store a token").await;
    assert!(
        failed.contains("classic personal access tokens are not supported"),
        "{failed}"
    );
    assert!(!failed.contains("ghp_abcdefghij"));
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn a_clean_exit_with_no_credential_file_says_so() {
    let harness = harness();
    let mut lines = prompt();
    lines.push("Waiting for authorization...".to_owned());
    lines.push("Signed in successfully as alice.".to_owned());
    harness.spawner.set(Script {
        lines,
        outcome: Outcome::ExitedClean,
    });

    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;
    harness.spawner.finish();

    let failed = harness.poll_fragment("could not store a token").await;
    assert!(failed.contains("wrote no credential file"), "{failed}");
    assert!(failed.contains("no config.json"), "{failed}");
    assert!(failed.contains("settings.json"), "{failed}");
    assert!(
        failed.contains("The container&#39;s last output:")
            || failed.contains("The container's last output:")
    );
    assert!(failed.contains("Signed in successfully as alice."));
    assert!(!failed.contains("did not finish"));
    assert!(harness.secrets.copilot_token().unwrap().is_none());
    assert!(!harness.spawner.homes()[0].exists());
}

#[tokio::test]
async fn directory_names_that_look_like_tokens_are_redacted_in_both_listings() {
    for outcome in [
        Outcome::ExitedClean,
        Outcome::Success(r#"{"authTokens":{}}"#.to_owned()),
    ] {
        let harness = harness();
        harness.spawner.set(Script {
            lines: prompt(),
            outcome,
        });
        harness.post("/settings/agents/copilot/login", "").await;
        harness.poll_fragment("1234-5678").await;
        let home = harness.spawner.homes()[0].clone();
        let inner = "ghu_innerabcdefghijklmnopqrstuvwxyz0123";
        std::fs::write(home.join(TOKEN), "").unwrap();
        std::fs::write(home.join(format!("credential:{TOKEN}")), "").unwrap();
        std::fs::create_dir(home.join(format!("dir:{TOKEN}"))).unwrap();
        std::fs::write(
            home.join(format!("dir:{TOKEN}")).join(format!("x:{inner}")),
            "",
        )
        .unwrap();
        std::fs::create_dir(home.join("logs")).unwrap();
        std::fs::write(home.join("logs").join(inner), "").unwrap();
        std::fs::create_dir(home.join(format!("{inner}-dir"))).unwrap();
        harness.spawner.finish();

        let failed = harness.poll_fragment("could not store a token").await;
        assert!(failed.contains("[redacted]"), "{failed}");
        assert!(failed.contains("logs/ [[redacted]]"), "{failed}");
        assert!(failed.contains("credential:[redacted]"), "{failed}");
        assert!(
            failed.contains("dir:[redacted]/ [x:[redacted]]"),
            "{failed}"
        );
        assert!(!failed.contains(TOKEN), "{failed}");
        assert!(!failed.contains("ghu_inner"), "{failed}");
    }
}

#[tokio::test]
async fn a_config_the_factory_cannot_read_shows_the_io_error() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::ExitedClean,
    });
    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;
    // A directory in place of the file fails to read for any user, root included.
    std::fs::create_dir(harness.spawner.homes()[0].join("config.json")).unwrap();
    harness.spawner.finish();

    let failed = harness.poll_fragment("could not store a token").await;
    assert!(failed.contains("could not read config.json"), "{failed}");
    assert!(failed.contains("Is a directory"), "{failed}");
}

#[tokio::test]
async fn a_config_that_is_not_json_shows_a_position_and_no_content() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Success(format!("\"{TOKEN}\" nope")),
    });
    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;
    harness.spawner.finish();

    let failed = harness.poll_fragment("could not store a token").await;
    assert!(failed.contains("not valid JSON"), "{failed}");
    assert!(!failed.contains(TOKEN));
}

#[tokio::test]
async fn a_failed_login_deletes_its_directory() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: prompt(),
        outcome: Outcome::Failure,
    });
    harness.post("/settings/agents/copilot/login", "").await;
    harness.poll_fragment("1234-5678").await;
    harness.spawner.finish();
    harness.poll_fragment("did not finish").await;
    assert!(!harness.spawner.homes()[0].exists());
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
