//! Tests the real `OpenRouter` client against a loopback server. They need
//! the `e2e-fakes` feature, which `just test` enables, for the base URL setter.
#![cfg(feature = "e2e-fakes")]

use std::sync::{Arc, Mutex};

use axum::{Router, http::HeaderMap, http::Method, routing::any};
use factory_server::{HttpOpenRouterApi, OpenRouterApi};

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
