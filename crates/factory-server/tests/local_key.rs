//! Tests for the personal Local API key page: open to every login, write-only,
//! kept per login as ciphertext and silent about the factory key.

#[path = "support/copilot_harness.rs"]
mod harness;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::Cli;
use harness::{Harness, Reply, harness, harness_unavailable};

const PAGE: &str = "/settings/local";
const BOB: &str = "bob@example.com";
const BOB_KEY: &str = "sk-bob-personal-key-1";
const FACTORY_KEY: &str = "sk-factory-wide-key-9";

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

async fn save(harness: &Harness, login: &str, key: &str) -> Reply {
    let page = as_login(harness, login, None).await.body;
    let server = harness
        .store
        .local_settings()
        .unwrap()
        .url
        .unwrap_or_default();
    save_for(harness, login, &csrf(&page), &server, key).await
}

async fn save_for(harness: &Harness, login: &str, token: &str, server: &str, key: &str) -> Reply {
    let server = server.replace(':', "%3A").replace('/', "%2F");
    as_login(
        harness,
        login,
        Some((PAGE, &format!("csrf={token}&server={server}&key={key}"))),
    )
    .await
}

async fn clear(harness: &Harness, login: &str) -> Reply {
    let token = csrf(&as_login(harness, login, None).await.body);
    as_login(
        harness,
        login,
        Some(("/settings/local/clear", &format!("csrf={token}"))),
    )
    .await
}

#[tokio::test]
async fn a_login_who_is_not_an_admin_sees_the_server_as_text() {
    let harness = harness();
    let none = as_login(&harness, BOB, None).await;
    assert_eq!(none.status, StatusCode::OK);
    assert!(none.body.contains("no local server is set"));
    assert!(none.body.contains(r#"href="/settings/local""#));
    assert!(none.body.contains("Local API key"));
    assert!(none.body.contains("the agent that runs there can read it"));

    harness
        .store
        .save_local_settings(Some("http://gfx1201:9931"), Cli::ClaudeCode, 10)
        .unwrap();
    harness.secrets.set_local_key(FACTORY_KEY).unwrap();
    let page = as_login(&harness, BOB, None).await;
    assert!(page.body.contains("http://gfx1201:9931"));
    assert!(page.body.contains("Claude Code"));
    assert!(!page.body.contains(r#"name="url""#));
    assert!(!page.body.contains(r#"name="cli""#));
    assert!(!page.body.contains(FACTORY_KEY));
    assert!(!page.body.to_lowercase().contains("factory key"));
    assert!(!page.body.to_lowercase().contains("factory's"));
}

#[tokio::test]
async fn the_key_is_write_only_kept_by_an_empty_save_and_cleared() {
    let harness = harness();
    assert!(
        as_login(&harness, BOB, None)
            .await
            .body
            .contains(r#"placeholder="not set""#)
    );
    let saved = save(&harness, BOB, BOB_KEY).await;
    assert!(saved.body.contains(r#"placeholder="set""#));
    assert!(!saved.body.contains(BOB_KEY));
    assert!(!as_login(&harness, BOB, None).await.body.contains(BOB_KEY));

    let (_, raw) = harness
        .store
        .get_secret(factory_server::SecretRef {
            project: "",
            server: "",
            header: &format!("local-key-user:{BOB}"),
        })
        .unwrap()
        .expect("the row");
    assert!(
        !raw.windows(BOB_KEY.len())
            .any(|window| window == BOB_KEY.as_bytes())
    );

    save(&harness, BOB, "").await;
    assert_eq!(
        harness.secrets.user_local_key(BOB).unwrap().as_deref(),
        Some(BOB_KEY)
    );

    let other = as_login(&harness, "carol@example.com", None).await;
    assert!(other.body.contains(r#"placeholder="not set""#));

    let cleared = clear(&harness, BOB).await;
    assert!(cleared.body.contains(r#"placeholder="not set""#));
    assert!(harness.secrets.user_local_key(BOB).unwrap().is_none());
}

#[tokio::test]
async fn a_post_without_the_form_token_is_refused() {
    let harness = harness();
    for path in [PAGE, "/settings/local/clear"] {
        let reply = as_login(&harness, BOB, Some((path, &format!("key={BOB_KEY}")))).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert!(harness.secrets.user_local_key(BOB).unwrap().is_none());
}

#[tokio::test]
async fn without_a_master_key_a_save_is_refused_and_clear_works() {
    let harness = harness_unavailable();
    let page = as_login(&harness, BOB, None).await;
    assert!(page.body.contains("The master key is missing"));
    let reply = save(&harness, BOB, BOB_KEY).await;
    assert!(reply.body.contains("was not saved"));
    assert_eq!(clear(&harness, BOB).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_key_from_a_page_of_another_server_is_not_saved() {
    let harness = harness();
    harness
        .store
        .save_local_settings(Some("http://old:1"), Cli::Codex, 10)
        .unwrap();
    let token = csrf(&as_login(&harness, BOB, None).await.body);
    harness
        .store
        .save_local_settings(Some("http://new:2"), Cli::Codex, 10)
        .unwrap();
    let stale = save_for(&harness, BOB, &token, "http://old:1", BOB_KEY).await;
    assert!(stale.body.contains("the key was not saved"));
    assert!(stale.body.contains("http://new:2"));
    assert!(harness.secrets.user_local_key(BOB).unwrap().is_none());

    let fresh = save_for(&harness, BOB, &token, "http://new:2", BOB_KEY).await;
    assert!(fresh.body.contains("Saved."));
    assert!(harness.secrets.user_local_key(BOB).unwrap().is_some());
}
