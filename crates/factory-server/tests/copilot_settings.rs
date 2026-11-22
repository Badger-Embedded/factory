//! Tests for the Copilot settings page: the stored-token state with no secret
//! in the HTML, the sidebar entry, the write-only paste field and Clear, and
//! the disabled state when the store is unavailable.

#[path = "support/copilot_harness.rs"]
mod harness;

use axum::http::StatusCode;
use harness::{harness, harness_unavailable};

const GHO: &str = "gho_abcdefghijklmnopqrstuvwxyz0123456789";
const PAT: &str = "github_pat_11ABCDEFG0abcdefghijklmnopqrstuvwxyz";

#[tokio::test]
async fn the_page_renders_with_its_heading_and_a_sidebar_entry() {
    let harness = harness();
    let page = harness.get("/settings/agents/copilot").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("<h1>Copilot</h1>"));
    assert!(page.body.contains("Sign in with GitHub"));
    assert!(page.body.contains("No token is stored."));
    assert!(page.body.contains("Copilot plan"));
    assert!(page.body.contains("moonthebot"));
    assert!(!page.body.contains("price"));

    let other = harness.get("/settings/workers").await;
    assert!(other.body.contains("href=\"/settings/agents/copilot\""));
}

#[tokio::test]
async fn the_page_never_shows_the_stored_token() {
    let harness = harness();
    harness.secrets.set_copilot_token(GHO).unwrap();

    let page = harness.get("/settings/agents/copilot").await;
    assert!(page.body.contains("A token is stored."));
    assert!(!page.body.contains(GHO));
}

#[tokio::test]
async fn pasted_tokens_are_stored_and_absent_from_the_html() {
    let harness = harness();
    for token in [GHO, PAT] {
        let reply = harness
            .post(
                "/settings/agents/copilot/token",
                &format!("token=%20{token}%0A"),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("Saved."));
        assert!(!reply.body.contains(token));
        assert_eq!(
            harness.secrets.copilot_token().unwrap().as_deref(),
            Some(token)
        );
    }
}

#[tokio::test]
async fn a_classic_token_is_refused_without_an_echo() {
    let harness = harness();
    let classic = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let reply = harness
        .post(
            "/settings/agents/copilot/token",
            &format!("token={classic}"),
        )
        .await;
    assert!(
        reply
            .body
            .contains("classic personal access tokens are not supported")
    );
    assert!(!reply.body.contains(classic));
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn an_unknown_format_is_refused_without_an_echo() {
    let harness = harness();
    let reply = harness
        .post("/settings/agents/copilot/token", "token=not-a-token-at-all")
        .await;
    assert!(reply.body.contains("must start with"));
    assert!(!reply.body.contains("not-a-token-at-all"));
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn an_empty_save_keeps_the_stored_token() {
    let harness = harness();
    harness.secrets.set_copilot_token(GHO).unwrap();
    let reply = harness
        .post("/settings/agents/copilot/token", "token=")
        .await;
    assert!(reply.body.contains("Saved."));
    assert_eq!(
        harness.secrets.copilot_token().unwrap().as_deref(),
        Some(GHO)
    );
}

#[tokio::test]
async fn clear_removes_the_stored_token() {
    let harness = harness();
    harness.secrets.set_copilot_token(GHO).unwrap();
    let reply = harness
        .post("/settings/agents/copilot/token/clear", "")
        .await;
    assert!(reply.body.contains("Cleared."));
    assert!(reply.body.contains("No token is stored."));
    assert!(harness.secrets.copilot_token().unwrap().is_none());
}

#[tokio::test]
async fn without_a_master_key_the_controls_are_disabled_and_clear_works() {
    let harness = harness_unavailable();
    let page = harness.get("/settings/agents/copilot").await;
    assert!(page.body.contains("The master key is missing"));
    assert!(page.body.contains("disabled"));

    let reply = harness
        .post("/settings/agents/copilot/token/clear", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Cleared."));
}
