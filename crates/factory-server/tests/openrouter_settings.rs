//! Tests for the `OpenRouter` agent settings page: the admin gate, the
//! write-only factory key, and the model list fetched through a fake API.

#[path = "support/copilot_harness.rs"]
mod harness;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::Cli;
use factory_server::{ModelsError, OpenRouterModel, parse_models};
use harness::{Harness, Reply, harness, harness_unavailable};

const PAGE: &str = "/settings/agents/openrouter";
const KEY: &str = "sk-or-v1-factory-key-123";

fn model(id: &str, name: &str) -> OpenRouterModel {
    OpenRouterModel {
        id: id.to_owned(),
        name: name.to_owned(),
    }
}

fn csrf(body: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..start + body[start..].find('"').unwrap()].to_owned()
}

async fn save(harness: &Harness, key: &str, cli: &str) -> Reply {
    let token = csrf(&harness.get(PAGE).await.body);
    harness
        .post(PAGE, &format!("csrf={token}&key={key}&cli={cli}"))
        .await
}

async fn post_token(harness: &Harness, path: &str) -> Reply {
    let token = csrf(&harness.get(PAGE).await.body);
    harness.post(path, &format!("csrf={token}")).await
}

fn set_models(harness: &Harness, models: Vec<OpenRouterModel>) {
    *harness.openrouter.answer.lock().unwrap() = Ok(models);
}

#[tokio::test]
async fn the_page_renders_for_an_admin_with_the_defaults() {
    let harness = harness();
    let page = harness.get(PAGE).await;
    assert_eq!(page.status, StatusCode::OK);
    for text in [
        "<h1>OpenRouter</h1>",
        "Factory API key",
        "Claude Code",
        "Copilot",
        "No models are stored.",
        "Refresh models",
        r#"href="https://openrouter.ai/keys" target="_blank" rel="noopener noreferrer""#,
    ] {
        assert!(page.body.contains(text), "{text}");
    }
    assert!(page.body.contains(r#"value="codex" checked"#));
    let other = harness.get("/settings/workers").await;
    assert!(other.body.contains(r#"href="/settings/agents/openrouter""#));
}

#[tokio::test]
async fn another_login_gets_403_on_every_route() {
    let harness = harness();
    for request in [
        Request::get(PAGE),
        Request::post(PAGE).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Request::post("/settings/agents/openrouter/refresh")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        Request::post("/settings/agents/openrouter/clear-key")
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
    }
    assert!(harness.openrouter.keys.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_post_without_the_form_token_is_refused() {
    let harness = harness();
    let reply = harness.post(PAGE, "cli=claude").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(harness.store.openrouter_settings().unwrap().cli, Cli::Codex);
}

#[tokio::test]
async fn a_save_stores_the_cli_and_an_unknown_cli_is_refused() {
    let harness = harness();
    let reply = save(&harness, "", "claude").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        harness.store.openrouter_settings().unwrap().cli,
        Cli::ClaudeCode,
    );
    let refused = save(&harness, "", "vim").await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        harness.store.openrouter_settings().unwrap().cli,
        Cli::ClaudeCode,
    );
}

#[tokio::test]
async fn the_factory_key_is_write_only_and_stored_as_ciphertext() {
    let harness = harness();
    let reply = save(&harness, KEY, "codex").await;
    assert!(reply.body.contains("A key is stored."));
    assert!(!reply.body.contains(KEY));
    assert!(!harness.get(PAGE).await.body.contains(KEY));
    let raw = harness
        .store
        .get_secret(factory_server::SecretRef {
            project: "",
            server: "",
            header: "openrouter-key",
        })
        .unwrap()
        .expect("the key row")
        .1;
    assert!(
        !raw.windows(KEY.len())
            .any(|window| window == KEY.as_bytes())
    );
    assert!(harness.secrets.local_key().unwrap().is_none());

    save(&harness, "", "codex").await;
    assert_eq!(
        harness.secrets.openrouter_key().unwrap().as_deref(),
        Some(KEY),
    );

    let cleared = post_token(&harness, "/settings/agents/openrouter/clear-key").await;
    assert!(cleared.body.contains("No key is stored."));
    assert!(harness.secrets.openrouter_key().unwrap().is_none());
}

#[tokio::test]
async fn without_a_master_key_a_key_is_refused_and_clear_works() {
    let harness = harness_unavailable();
    assert!(
        harness
            .get(PAGE)
            .await
            .body
            .contains("The master key is missing")
    );
    let reply = save(&harness, KEY, "claude").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(harness.store.openrouter_settings().unwrap().cli, Cli::Codex);
    let cleared = post_token(&harness, "/settings/agents/openrouter/clear-key").await;
    assert_eq!(cleared.status, StatusCode::OK);
}

#[tokio::test]
async fn without_a_master_key_the_list_is_still_fetched_without_a_key() {
    let harness = harness_unavailable();
    set_models(&harness, vec![model("a/b", "A B")]);
    let reply = save(&harness, "", "codex").await;
    assert!(reply.body.contains("OpenRouter lists 1 models"));
    assert_eq!(*harness.openrouter.keys.lock().unwrap(), [None]);
}

#[tokio::test]
async fn an_unreadable_key_row_sends_no_request_and_keeps_the_list() {
    let harness = harness_unavailable();
    harness
        .store
        .put_secret(
            factory_server::SecretRef {
                project: "",
                server: "",
                header: "openrouter-key",
            },
            "local",
            b"sealed",
        )
        .unwrap();
    let page = harness.get(PAGE).await;
    assert!(page.body.contains("The stored key cannot be read"));
    let reply = post_token(&harness, "/settings/agents/openrouter/refresh").await;
    assert!(reply.body.contains("The earlier list stays."));
    assert!(harness.openrouter.keys.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_save_and_a_refresh_store_the_models_and_show_count_and_time() {
    let harness = harness();
    set_models(
        &harness,
        vec![
            model(
                "anthropic/claude-sonnet-4.5",
                "Anthropic: Claude Sonnet 4.5",
            ),
            model("openai/gpt-5", "OpenAI: GPT-5"),
        ],
    );
    let reply = save(&harness, KEY, "codex").await;
    assert!(reply.body.contains("OpenRouter lists 2 models"));
    assert!(
        reply
            .body
            .contains("2 models that support tools are stored.")
    );
    assert!(reply.body.contains("Fetched "));
    assert_eq!(
        *harness.openrouter.keys.lock().unwrap(),
        [Some(KEY.to_owned())]
    );
    let stored = harness.store.openrouter_models().unwrap();
    assert_eq!(
        stored[0],
        model(
            "anthropic/claude-sonnet-4.5",
            "Anthropic: Claude Sonnet 4.5"
        )
    );
    assert_eq!(stored.len(), 2);
    assert!(
        harness
            .store
            .openrouter_settings()
            .unwrap()
            .models_fetched_at
            .is_some()
    );

    set_models(&harness, vec![model("x/y", "X Y")]);
    let refreshed = post_token(&harness, "/settings/agents/openrouter/refresh").await;
    assert!(
        refreshed
            .body
            .contains("1 models that support tools are stored.")
    );
    assert_eq!(
        harness.store.openrouter_models().unwrap(),
        [model("x/y", "X Y")]
    );
}

#[tokio::test]
async fn a_failed_fetch_shows_its_reason_and_keeps_the_earlier_list() {
    let harness = harness();
    set_models(&harness, vec![model("a/b", "A B")]);
    save(&harness, "", "codex").await;
    let fetched = harness
        .store
        .openrouter_settings()
        .unwrap()
        .models_fetched_at;

    for (error, text) in [
        (ModelsError::Status(500), "the server answered HTTP 500"),
        (ModelsError::Timeout, "did not answer within 10 seconds"),
        (ModelsError::NotAModelList, "not a model list"),
    ] {
        *harness.openrouter.answer.lock().unwrap() = Err(error);
        let reply = post_token(&harness, "/settings/agents/openrouter/refresh").await;
        assert!(reply.body.contains(text), "{text}");
        assert!(reply.body.contains("The earlier list stays."));
        assert_eq!(
            harness.store.openrouter_models().unwrap(),
            [model("a/b", "A B")]
        );
        assert_eq!(
            harness
                .store
                .openrouter_settings()
                .unwrap()
                .models_fetched_at,
            fetched,
        );
    }
}

#[test]
fn a_malformed_tools_field_is_refused_and_an_omitted_one_is_not() {
    let malformed = br#"{"data":[{"id":"a/b","supported_parameters":"tools"}]}"#;
    assert_eq!(parse_models(malformed), Err(ModelsError::NotAModelList));
    let omitted = br#"{"data":[{"id":"a/b"}]}"#;
    assert_eq!(parse_models(omitted), Ok(vec![]));
}

#[tokio::test]
async fn a_malformed_list_keeps_the_earlier_list_and_fetch_time() {
    let harness = harness();
    set_models(&harness, vec![model("a/b", "A B")]);
    save(&harness, "", "codex").await;
    let fetched = harness
        .store
        .openrouter_settings()
        .unwrap()
        .models_fetched_at;
    *harness.openrouter.answer.lock().unwrap() =
        parse_models(br#"{"data":[{"id":"x","supported_parameters":"tools"}]}"#);
    let reply = post_token(&harness, "/settings/agents/openrouter/refresh").await;
    assert!(reply.body.contains("The earlier list stays."));
    assert_eq!(
        harness.store.openrouter_models().unwrap(),
        [model("a/b", "A B")]
    );
    assert_eq!(
        harness
            .store
            .openrouter_settings()
            .unwrap()
            .models_fetched_at,
        fetched
    );
}

#[tokio::test]
async fn a_list_that_holds_the_key_is_refused() {
    let harness = harness();
    save(&harness, KEY, "codex").await;
    set_models(&harness, vec![model(KEY, "Leaky")]);
    let reply = post_token(&harness, "/settings/agents/openrouter/refresh").await;
    assert!(reply.body.contains("The earlier list stays."));
    assert!(!reply.body.contains(KEY));
    assert_eq!(harness.store.openrouter_models().unwrap().len(), 0);
}

#[test]
fn only_models_that_support_tools_are_kept() {
    let body = br#"{"data":[
        {"id":"a/tools","name":"A Tools","supported_parameters":["temperature","tools"]},
        {"id":"b/plain","name":"B Plain","supported_parameters":["temperature"]},
        {"id":"c/none","name":"C None"},
        {"id":"d/unnamed","supported_parameters":["tools"]}
    ]}"#;
    let models = parse_models(body).unwrap();
    let listed: Vec<_> = models.iter().map(|m| (&*m.id, &*m.name)).collect();
    assert_eq!(listed, [("a/tools", "A Tools"), ("d/unnamed", "d/unnamed")]);
}

#[test]
fn a_body_that_is_no_model_list_is_refused() {
    for body in [&b"{}"[..], b"not json", br#"{"data":[{"name":"x"}]}"#] {
        assert_eq!(parse_models(body), Err(ModelsError::NotAModelList));
    }
}
