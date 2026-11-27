//! Tests the real `OpenRouter` client against a loopback server. They need
//! the `e2e-fakes` feature, which `just test` enables, for the base URL setter.
#![cfg(feature = "e2e-fakes")]

use std::sync::{Arc, Mutex};

use axum::{Router, http::HeaderMap, http::Method, routing::any};
use factory_server::{GenerationError, HttpOpenRouterApi, OpenRouterApi};

#[tokio::test]
async fn the_client_sends_one_bearer_get_to_the_key_endpoint() {
    let seen = Arc::new(Mutex::new(Vec::<(Method, Option<String>)>::new()));
    let log = Arc::clone(&seen);
    let app = Router::new().route(
        "/api/v1/key",
        any(move |method: Method, headers: HeaderMap| {
            let log = Arc::clone(&log);
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                log.lock().unwrap().push((method, auth));
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    HttpOpenRouterApi::with_base_url(&url)
        .check_key("sk-or-v1-key")
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![(Method::GET, Some("Bearer sk-or-v1-key".to_owned()))]
    );
}

#[tokio::test]
async fn the_client_reads_a_generations_total_cost_and_treats_a_404_as_not_ready() {
    let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let log = Arc::clone(&seen);
    let app = Router::new().route(
        "/api/v1/generation",
        axum::routing::get(
            move |axum::extract::Query(query): axum::extract::Query<
                std::collections::HashMap<String, String>,
            >,
                  headers: HeaderMap| {
                let log = Arc::clone(&log);
                async move {
                    let id = query["id"].clone();
                    let auth = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    log.lock().unwrap().push((id.clone(), auth));
                    match id.as_str() {
                        "gen-1" => (
                            axum::http::StatusCode::OK,
                            r#"{"data":{"total_cost":0.125}}"#,
                        ),
                        "gen-empty" => (axum::http::StatusCode::OK, r#"{"data":{}}"#),
                        "gen-denied" => (axum::http::StatusCode::FORBIDDEN, ""),
                        _ => (axum::http::StatusCode::NOT_FOUND, ""),
                    }
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = HttpOpenRouterApi::with_base_url(&url);

    assert_eq!(api.generation("gen-1", "sk-or-v1-key").await, Ok(0.125));
    assert_eq!(
        api.generation("gen-2", "sk-or-v1-key").await,
        Err(GenerationError::NotReady)
    );
    assert_eq!(
        api.generation("gen-empty", "sk-or-v1-key").await,
        Err(GenerationError::NotReady)
    );
    let denied = api
        .generation("gen-denied", "sk-or-v1-key")
        .await
        .unwrap_err();
    assert!(!denied.retryable());
    assert!(!denied.to_string().contains("sk-or"));
    assert_eq!(
        seen.lock().unwrap()[0],
        ("gen-1".to_owned(), Some("Bearer sk-or-v1-key".to_owned()))
    );
}
