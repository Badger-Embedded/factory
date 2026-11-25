//! Tests for the Local agent settings page: the admin gate, the URL and timeout
//! checks, the write-only factory key, and the model list fetched from a stub
//! server.

#[path = "support/copilot_harness.rs"]
mod harness;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::get;
use factory_core::Cli;
use harness::{Harness, Reply, harness, harness_unavailable};

const PAGE: &str = "/settings/agents/local";
const KEY: &str = "sk-local-factory-key-123";

#[derive(Clone, Copy)]
enum Answer {
    Models,
    Broken,
    NotAList,
}

struct Stub {
    url: String,
    auth: Arc<Mutex<Vec<Option<String>>>>,
    answer: Arc<Mutex<Answer>>,
    ids: Arc<Mutex<Vec<String>>>,
    delay_ms: Arc<Mutex<u64>>,
}

async fn stub() -> Stub {
    #[derive(Clone)]
    struct Shared {
        auth: Arc<Mutex<Vec<Option<String>>>>,
        answer: Arc<Mutex<Answer>>,
        ids: Arc<Mutex<Vec<String>>>,
        delay_ms: Arc<Mutex<u64>>,
    }
    async fn models(State(shared): State<Shared>, headers: HeaderMap) -> (StatusCode, String) {
        shared.auth.lock().unwrap().push(
            headers
                .get(header::AUTHORIZATION)
                .map(|value| value.to_str().unwrap().to_owned()),
        );
        let delay = *shared.delay_ms.lock().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        let answer = *shared.answer.lock().unwrap();
        match answer {
            Answer::Models => {
                let data: Vec<String> = shared
                    .ids
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|id| format!(r#"{{"id":"{id}"}}"#))
                    .collect();
                (
                    StatusCode::OK,
                    format!(r#"{{"object":"list","data":[{}]}}"#, data.join(",")),
                )
            }
            Answer::Broken => (StatusCode::INTERNAL_SERVER_ERROR, "boom".into()),
            Answer::NotAList => (StatusCode::OK, r#"{"hello":"world"}"#.into()),
        }
    }
    let shared = Shared {
        auth: Arc::default(),
        answer: Arc::new(Mutex::new(Answer::Models)),
        ids: Arc::new(Mutex::new(vec![
            "gemma-4-26B".into(),
            "Llama-3.2-1B".into(),
        ])),
        delay_ms: Arc::default(),
    };
    let app = Router::new()
        .route("/v1/models", get(models))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Stub {
        url,
        auth: shared.auth,
        answer: shared.answer,
        ids: shared.ids,
        delay_ms: shared.delay_ms,
    }
}

fn csrf(body: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..start + body[start..].find('"').unwrap()].to_owned()
}

fn form(token: &str, url: &str, key: &str, cli: &str, timeout: &str) -> String {
    let enc = |text: &str| {
        text.replace(':', "%3A")
            .replace('/', "%2F")
            .replace('?', "%3F")
    };
    format!(
        "csrf={}&url={}&key={}&cli={cli}&load_timeout={timeout}",
        enc(token),
        enc(url),
        enc(key),
    )
}

async fn save(harness: &Harness, url: &str, key: &str, cli: &str, timeout: &str) -> Reply {
    let token = csrf(&harness.get(PAGE).await.body);
    harness
        .post(PAGE, &form(&token, url, key, cli, timeout))
        .await
}

async fn post_token(harness: &Harness, path: &str) -> Reply {
    let token = csrf(&harness.get(PAGE).await.body);
    harness.post(path, &format!("csrf={token}")).await
}

fn raw_key_row(harness: &Harness) -> Vec<u8> {
    harness
        .store
        .get_secret(factory_server::SecretRef {
            project: "",
            server: "",
            header: "local-key",
        })
        .unwrap()
        .expect("the key row")
        .1
}

#[tokio::test]
async fn the_page_renders_for_an_admin_with_the_defaults() {
    let harness = harness();
    let page = harness.get(PAGE).await;
    assert_eq!(page.status, StatusCode::OK);
    for text in [
        "<h1>Local</h1>",
        "Server URL",
        "Factory API key",
        "Claude Code",
        "Codex",
        "Copilot",
    ] {
        assert!(page.body.contains(text), "{text}");
    }
    assert!(page.body.contains(r#"<option value="codex" selected"#));
    assert!(page.body.contains(r#"value="10""#));
    assert!(page.body.contains("the agent that runs there can read it"));
    let other = harness.get("/settings/workers").await;
    assert!(other.body.contains(r#"href="/settings/agents/local""#));
}

#[tokio::test]
async fn another_login_gets_403_on_get_and_post() {
    let harness = harness();
    for request in [
        Request::get(PAGE),
        Request::post(PAGE).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Request::post("/settings/agents/local/refresh")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Request::post("/settings/agents/local/clear-key")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
    ] {
        let reply = harness
            .send(
                request
                    .header(factory_server::USER_HEADER, "bob@example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert!(reply.body.contains("This page is for factory admins."));
    }
}

#[tokio::test]
async fn a_post_without_the_form_token_is_refused() {
    let harness = harness();
    let reply = harness
        .post(PAGE, "url=http%3A%2F%2Fh%3A1&cli=codex&load_timeout=10")
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(harness.store.local_settings().unwrap().url, None);
}

#[tokio::test]
async fn urls_are_stored_in_normal_form() {
    let harness = harness();
    for input in [
        "http://127.0.0.1:1",
        "http://127.0.0.1:1/",
        "http://127.0.0.1:1/v1/",
    ] {
        let reply = save(&harness, input, "", "claude", "5").await;
        assert_eq!(reply.status, StatusCode::OK);
        let stored = harness.store.local_settings().unwrap();
        assert_eq!(stored.url.as_deref(), Some("http://127.0.0.1:1"));
        assert_eq!(stored.cli, Cli::ClaudeCode);
        assert_eq!(stored.load_timeout_minutes, 5);
    }
}

#[tokio::test]
async fn refused_saves_write_nothing_and_name_the_problem() {
    let harness = harness();
    save(&harness, "http://127.0.0.1:1", "", "codex", "10").await;
    let before = harness.store.local_settings().unwrap();
    for (url, timeout, problem) in [
        ("ftp://x", "10", "must start with http"),
        ("http://user:pw@host", "10", "user name or password"),
        ("http://host/?a=1", "10", "must not hold a query"),
        ("http://host/#a", "10", "must not hold a fragment"),
        ("http://host", "0", "1 to 60"),
        ("http://host", "61", "1 to 60"),
        ("http://host", "soon", "1 to 60"),
    ] {
        let reply = save(&harness, url, KEY, "copilot", timeout).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{url} {timeout}");
        assert!(reply.body.contains(problem), "{url} {timeout}");
        assert!(!reply.body.contains(KEY));
        assert_eq!(harness.store.local_settings().unwrap(), before);
        assert!(harness.secrets.local_key().unwrap().is_none());
    }
}

#[tokio::test]
async fn the_factory_key_is_write_only_and_stored_as_ciphertext() {
    let harness = harness();
    let reply = save(&harness, "http://127.0.0.1:1", KEY, "codex", "10").await;
    assert!(reply.body.contains("A key is stored."));
    assert!(!reply.body.contains(KEY));
    assert!(!harness.get(PAGE).await.body.contains(KEY));
    let raw = raw_key_row(&harness);
    assert!(
        !raw.windows(KEY.len())
            .any(|window| window == KEY.as_bytes())
    );

    save(&harness, "http://127.0.0.1:1", "", "codex", "10").await;
    assert_eq!(harness.secrets.local_key().unwrap().as_deref(), Some(KEY));

    let cleared = post_token(&harness, "/settings/agents/local/clear-key").await;
    assert!(cleared.body.contains("No key is stored."));
    assert!(harness.secrets.local_key().unwrap().is_none());
}

#[tokio::test]
async fn without_a_master_key_a_key_is_refused_and_clear_works() {
    let harness = harness_unavailable();
    let page = harness.get(PAGE).await;
    assert!(page.body.contains("The master key is missing"));
    let reply = save(&harness, "http://127.0.0.1:1", KEY, "codex", "10").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(harness.store.local_settings().unwrap().url, None);
    let cleared = post_token(&harness, "/settings/agents/local/clear-key").await;
    assert_eq!(cleared.status, StatusCode::OK);
}

#[tokio::test]
async fn without_a_master_key_a_keyless_server_is_still_fetched() {
    let harness = harness_unavailable();
    let stub = stub().await;
    let reply = save(&harness, &stub.url, "", "codex", "10").await;
    assert!(reply.body.contains("The server lists 2 models."));
    assert!(!reply.body.contains("cannot be read"));
    assert_eq!(*stub.auth.lock().unwrap(), [None]);
}

#[tokio::test]
async fn without_a_master_key_a_stored_key_row_is_unreadable_and_not_sent() {
    let harness = harness_unavailable();
    let stub = stub().await;
    save(&harness, &stub.url, "", "codex", "10").await;
    harness
        .store
        .put_secret(
            factory_server::SecretRef {
                project: "",
                server: "",
                header: "local-key",
            },
            "local",
            b"sealed",
        )
        .unwrap();
    let page = harness.get(PAGE).await;
    assert!(page.body.contains("A key is stored."));
    assert!(page.body.contains("The stored key cannot be read"));
    let reply = post_token(&harness, "/settings/agents/local/refresh").await;
    assert!(reply.body.contains("The earlier list stays."));
    assert_eq!(stub.auth.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_save_stores_the_models_the_server_lists() {
    let harness = harness();
    let stub = stub().await;
    let reply = save(&harness, &format!("{}/v1/", stub.url), "", "codex", "10").await;
    assert!(reply.body.contains("The server lists 2 models."));
    assert!(reply.body.contains("gemma-4-26B"));
    assert_eq!(
        harness.store.local_models().unwrap(),
        ["gemma-4-26B", "Llama-3.2-1B"],
    );
    assert!(
        harness
            .store
            .local_settings()
            .unwrap()
            .models_fetched_at
            .is_some()
    );
    assert_eq!(*stub.auth.lock().unwrap(), [None]);
}

#[tokio::test]
async fn the_stored_key_is_sent_as_a_bearer_token() {
    let harness = harness();
    let stub = stub().await;
    save(&harness, &stub.url, KEY, "codex", "10").await;
    save(&harness, &stub.url, "", "codex", "10").await;
    post_token(&harness, "/settings/agents/local/refresh").await;
    let bearer = Some(format!("Bearer {KEY}"));
    assert_eq!(
        *stub.auth.lock().unwrap(),
        [bearer.clone(), bearer.clone(), bearer]
    );

    post_token(&harness, "/settings/agents/local/clear-key").await;
    post_token(&harness, "/settings/agents/local/refresh").await;
    assert_eq!(stub.auth.lock().unwrap().last().unwrap(), &None);
}

#[tokio::test]
async fn a_failed_fetch_keeps_the_settings_and_the_earlier_list() {
    let harness = harness();
    let stub = stub().await;
    save(&harness, &stub.url, KEY, "codex", "10").await;
    let fetched = harness.store.local_settings().unwrap().models_fetched_at;

    for (answer, reason) in [
        (Answer::Broken, "HTTP 500"),
        (Answer::NotAList, "not a model list"),
    ] {
        *stub.answer.lock().unwrap() = answer;
        let reply = save(&harness, &stub.url, "", "copilot", "7").await;
        assert!(reply.body.contains(reason), "{reason}");
        assert!(reply.body.contains("The earlier list stays."));
        assert!(!reply.body.contains(KEY));
        let stored = harness.store.local_settings().unwrap();
        assert_eq!((stored.cli, stored.load_timeout_minutes), (Cli::Copilot, 7));
        assert_eq!(stored.models_fetched_at, fetched);
        assert_eq!(harness.store.local_models().unwrap().len(), 2);
    }

    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let reply = save(&harness, &url, "", "copilot", "7").await;
    assert!(reply.body.contains("cannot connect"));
    assert_eq!(
        harness.store.local_settings().unwrap().url.as_deref(),
        Some(url.as_str())
    );
    assert_eq!(harness.store.local_models().unwrap().len(), 2);
}

#[tokio::test]
async fn refresh_fetches_without_changing_the_settings() {
    let harness = harness();
    let stub = stub().await;
    save(&harness, &stub.url, "", "claude", "3").await;
    let reply = post_token(&harness, "/settings/agents/local/refresh").await;
    assert!(reply.body.contains("The server lists 2 models."));
    let stored = harness.store.local_settings().unwrap();
    assert_eq!(
        (stored.cli, stored.load_timeout_minutes),
        (Cli::ClaudeCode, 3)
    );
}

#[tokio::test]
async fn overlapping_saves_do_not_mix_one_server_with_another_key() {
    let harness = harness();
    let slow = stub().await;
    let fast = stub().await;
    *slow.ids.lock().unwrap() = vec!["slow-model".into()];
    *slow.delay_ms.lock().unwrap() = 300;
    *fast.ids.lock().unwrap() = vec!["fast-model".into()];
    let first = save(&harness, &slow.url, "key-for-slow", "codex", "10");
    let second = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        save(&harness, &fast.url, "key-for-fast", "codex", "10").await
    };
    tokio::join!(first, second);

    assert_eq!(harness.store.local_models().unwrap(), ["fast-model"]);
    assert_eq!(
        harness.store.local_settings().unwrap().url.as_deref(),
        Some(fast.url.as_str())
    );
    assert_eq!(
        *slow.auth.lock().unwrap(),
        [Some("Bearer key-for-slow".to_owned())]
    );
    assert_eq!(
        *fast.auth.lock().unwrap(),
        [Some("Bearer key-for-fast".to_owned())]
    );
}

#[tokio::test]
async fn an_unreadable_key_is_reported_and_keeps_the_list() {
    let harness = harness();
    let stub = stub().await;
    save(&harness, &stub.url, KEY, "codex", "10").await;
    let (backend, mut bytes) = harness
        .store
        .get_secret(factory_server::SecretRef {
            project: "",
            server: "",
            header: "local-key",
        })
        .unwrap()
        .unwrap();
    bytes.truncate(bytes.len() / 2);
    harness
        .store
        .put_secret(
            factory_server::SecretRef {
                project: "",
                server: "",
                header: "local-key",
            },
            &backend,
            &bytes,
        )
        .unwrap();
    *stub.ids.lock().unwrap() = vec!["other".into()];

    let page = harness.get(PAGE).await;
    assert!(page.body.contains("The stored key cannot be read"));
    let reply = post_token(&harness, "/settings/agents/local/refresh").await;
    assert!(reply.body.contains("The earlier list stays."));
    assert_eq!(
        harness.store.local_models().unwrap(),
        ["gemma-4-26B", "Llama-3.2-1B"]
    );
    assert_eq!(stub.auth.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_model_list_that_holds_the_key_is_rejected_unseen() {
    let harness = harness();
    let stub = stub().await;
    save(&harness, &stub.url, KEY, "codex", "10").await;
    *stub.ids.lock().unwrap() = vec![format!("model-{KEY}")];
    let reply = post_token(&harness, "/settings/agents/local/refresh").await;
    assert!(reply.body.contains("returned the key as a model id"));
    assert!(reply.body.contains("The earlier list stays."));
    assert!(!reply.body.contains(KEY));
    assert_eq!(
        harness.store.local_models().unwrap(),
        ["gemma-4-26B", "Llama-3.2-1B"]
    );
}

#[test]
fn urls_normalize_to_the_origin_and_path() {
    use factory_server::normalize_server_url;
    for input in [
        "http://gfx1201:9931",
        "http://gfx1201:9931/",
        "http://gfx1201:9931/v1",
        "http://gfx1201:9931/v1/",
    ] {
        assert_eq!(
            normalize_server_url(input).as_deref(),
            Ok("http://gfx1201:9931")
        );
    }
    assert_eq!(
        normalize_server_url("https://h/llm/v1/").as_deref(),
        Ok("https://h/llm")
    );
    for input in [
        "ftp://x",
        "http://user:pw@host",
        "http://host/?a=1",
        "http://host/#f",
        "nope",
    ] {
        assert!(normalize_server_url(input).is_err(), "{input}");
    }
}
