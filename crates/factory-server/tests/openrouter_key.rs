//! Tests for the personal `OpenRouter` page: open to every login, a write-only
//! key and a CLI choice kept per login, silent about the factory key.

#[path = "support/copilot_harness.rs"]
mod harness;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::{Agent, Cli};
use harness::{Harness, Reply, harness, harness_unavailable};

const PAGE: &str = "/settings/openrouter";
const BOB: &str = "bob@example.com";
const CAROL: &str = "carol@example.com";
const BOB_KEY: &str = "sk-or-bob-personal-key-1";
const FACTORY_KEY: &str = "sk-or-factory-wide-key-9";

async fn as_login(harness: &Harness, login: &str, post: Option<(&str, &str)>) -> Reply {
    let request = match post {
        None => Request::get(PAGE).body(Body::empty()),
        Some((path, form)) => Request::post(path)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.to_owned())),
    };
    let mut request = request.unwrap();
    request
        .headers_mut()
        .insert(factory_server::USER_HEADER, login.parse().unwrap());
    harness.send(request).await
}

fn csrf(body: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..start + body[start..].find('"').unwrap()].to_owned()
}

async fn save(harness: &Harness, login: &str, fields: &str) -> Reply {
    let token = csrf(&as_login(harness, login, None).await.body);
    as_login(
        harness,
        login,
        Some((PAGE, &format!("csrf={token}&{fields}"))),
    )
    .await
}

async fn clear(harness: &Harness, login: &str) -> Reply {
    let token = csrf(&as_login(harness, login, None).await.body);
    as_login(
        harness,
        login,
        Some(("/settings/openrouter/clear", &format!("csrf={token}"))),
    )
    .await
}

#[tokio::test]
async fn a_login_who_is_not_an_admin_opens_the_page_and_never_sees_the_factory_key() {
    let harness = harness();
    harness.secrets.set_openrouter_key(FACTORY_KEY).unwrap();
    harness
        .store
        .save_openrouter_settings(Cli::ClaudeCode)
        .unwrap();
    let page = as_login(&harness, BOB, None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(r#"href="/settings/openrouter""#));
    assert!(page.body.contains("<h1>OpenRouter</h1>"));
    assert!(page.body.contains("Factory default (Claude Code)"));
    assert!(page.body.contains(r#"placeholder="not set""#));
    assert!(page.body.contains(r#"rel="noopener noreferrer""#));
    assert!(page.body.contains("the agent that runs there can read it"));
    assert!(!page.body.contains(FACTORY_KEY));
    assert!(!page.body.to_lowercase().contains("factory key"));
    assert!(!page.body.to_lowercase().contains("factory's"));
}

#[tokio::test]
async fn the_key_is_write_only_kept_by_an_empty_save_and_cleared() {
    let harness = harness();
    let saved = save(&harness, BOB, &format!("key={BOB_KEY}")).await;
    assert!(saved.body.contains(r#"placeholder="set""#));
    assert!(!saved.body.contains(BOB_KEY));
    assert!(!as_login(&harness, BOB, None).await.body.contains(BOB_KEY));

    let (_, raw) = harness
        .store
        .get_secret(factory_server::SecretRef {
            project: "",
            server: "",
            header: &format!("openrouter-key-user:{BOB}"),
        })
        .unwrap()
        .expect("the row");
    assert!(
        !raw.windows(BOB_KEY.len())
            .any(|window| window == BOB_KEY.as_bytes())
    );

    save(&harness, BOB, "key=").await;
    assert_eq!(
        harness.secrets.user_openrouter_key(BOB).unwrap().as_deref(),
        Some(BOB_KEY)
    );
    assert!(harness.secrets.user_local_key(BOB).unwrap().is_none());

    let cleared = clear(&harness, BOB).await;
    assert!(cleared.body.contains(r#"placeholder="not set""#));
    assert!(harness.secrets.user_openrouter_key(BOB).unwrap().is_none());
}

#[tokio::test]
async fn another_logins_key_and_cli_rows_stay_untouched() {
    let harness = harness();
    save(&harness, CAROL, "key=sk-or-carol&cli=codex").await;
    save(&harness, BOB, &format!("key={BOB_KEY}&cli=claude")).await;
    clear(&harness, BOB).await;

    assert_eq!(
        harness
            .secrets
            .user_openrouter_key(CAROL)
            .unwrap()
            .as_deref(),
        Some("sk-or-carol")
    );
    assert_eq!(
        harness.store.user_cli(CAROL, Agent::OpenRouter).unwrap(),
        Some(Cli::Codex)
    );
    assert_eq!(
        harness.store.user_cli(BOB, Agent::OpenRouter).unwrap(),
        Some(Cli::ClaudeCode)
    );
    assert_eq!(harness.store.user_cli(BOB, Agent::Local).unwrap(), None);
    let other = as_login(&harness, "dave@example.com", None).await;
    assert!(other.body.contains(r#"placeholder="not set""#));
}

#[tokio::test]
async fn a_login_picks_its_cli_without_a_master_key() {
    let harness = harness_unavailable();
    let picked = save(&harness, BOB, "key=&cli=claude").await;
    assert!(picked.body.contains("Saved."));
    assert!(picked.body.contains(r#"value="claude" selected"#));
    assert_eq!(
        harness.store.user_cli(BOB, Agent::OpenRouter).unwrap(),
        Some(Cli::ClaudeCode)
    );

    save(&harness, BOB, "key=").await;
    assert_eq!(
        harness.store.user_cli(BOB, Agent::OpenRouter).unwrap(),
        Some(Cli::ClaudeCode)
    );

    save(&harness, BOB, "key=&cli=default").await;
    assert_eq!(
        harness.store.user_cli(BOB, Agent::OpenRouter).unwrap(),
        None
    );

    let bad = save(&harness, BOB, "key=&cli=nope").await;
    assert!(bad.body.contains("Unknown CLI."));
}

#[tokio::test]
async fn a_post_without_the_form_token_is_refused() {
    let harness = harness();
    for path in [PAGE, "/settings/openrouter/clear"] {
        let reply = as_login(&harness, BOB, Some((path, &format!("key={BOB_KEY}")))).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert!(harness.secrets.user_openrouter_key(BOB).unwrap().is_none());
}

#[tokio::test]
async fn without_a_master_key_a_save_is_refused_and_clear_works() {
    let harness = harness_unavailable();
    let page = as_login(&harness, BOB, None).await;
    assert!(page.body.contains("The master key is missing"));
    let reply = save(&harness, BOB, &format!("key={BOB_KEY}")).await;
    assert!(reply.body.contains("was not saved"));
    assert_eq!(clear(&harness, BOB).await.status, StatusCode::OK);
}
