//! Tests for the real MCP connection check client, [`HttpMcpChecker`]. A local
//! fake MCP server stands in for a real one, so the tests reach only
//! `127.0.0.1` and open no outside connection. Most tests connect over plain
//! HTTP. One serves TLS with a test CA to check `FACTORY_MCP_CA_FILE`'s roots.

use std::sync::{Arc, Mutex};
use std::time::Duration;

mod support;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use factory_server::{CheckOutcome, HttpMcpChecker, McpChecker, McpTool, ToolList};
use tokio::net::TcpListener;

/// One request the fake server received.
#[derive(Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<Seen>>,
}

impl Fake {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// A JSON-RPC `initialize` result, the shape a server answers the handshake
/// with.
const INIT_JSON: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"fake","version":"9.9"}}}"#;

fn record(fake: &Fake, request: &Request) {
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    fake.seen.lock().unwrap().push(Seen {
        method: request.method().to_string(),
        path: request.uri().path().to_owned(),
        headers,
    });
}

/// Answers the `initialize` POST with JSON and a session id, and records the
/// DELETE the checker sends to end the session.
async fn mcp_json(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (HeaderName::from_static("mcp-session-id"), "sess-1"),
        ],
        INIT_JSON,
    )
        .into_response()
}

/// Answers with an SSE stream whose first `data:` event carries the result.
async fn mcp_sse(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    let body = format!("event: message\ndata: {INIT_JSON}\n\n");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn unauthorized(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    (StatusCode::UNAUTHORIZED, "no").into_response()
}

async fn not_mcp(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"hello":"world"}"#,
    )
        .into_response()
}

/// Answers like a server that validates `initialize` strictly: a JSON-RPC
/// error over SSE when `clientInfo.version` is missing, the result otherwise.
/// The error echoes the request's `x-key` header, as a server may.
async fn strict(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    let key = request
        .headers()
        .get("x-key")
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let data = if body["params"]["clientInfo"]["version"].is_string() {
        INIT_JSON.to_owned()
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32603, "message": format!("clientInfo.version missing, key {key}")},
        })
        .to_string()
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(axum::body::Body::from(format!(
            ": connected\n\nevent: message\ndata: {data}\n\n"
        )))
        .unwrap()
}

/// Always answers with a JSON-RPC error that echoes the `x-key` header.
async fn rpc_error(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    let key = request
        .headers()
        .get("x-key")
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32602, "message": format!("bad key {key} {}", "x".repeat(400))},
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn redirect(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    (
        StatusCode::FOUND,
        [(header::LOCATION, "/elsewhere")],
        "moved",
    )
        .into_response()
}

async fn slow(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    tokio::time::sleep(Duration::from_secs(3)).await;
    (StatusCode::OK, INIT_JSON).into_response()
}

/// The `initialize` result of a server that declares the `tools` capability.
const INIT_TOOLS: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fake"}}}"#;

/// The JSON-RPC method of a request body, and its `params.cursor`.
async fn method_and_cursor(request: Request) -> (String, Option<String>) {
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    (
        body["method"].as_str().unwrap_or_default().to_owned(),
        body["params"]["cursor"].as_str().map(str::to_owned),
    )
}

/// A server with two pages of tools: the first as JSON with a cursor, the
/// second as SSE. `notifications/initialized` gets a 202.
async fn with_tools(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    if request.method() == "DELETE" {
        return StatusCode::OK.into_response();
    }
    let (method, cursor) = method_and_cursor(request).await;
    match (method.as_str(), cursor.as_deref()) {
        ("initialize", _) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (HeaderName::from_static("mcp-session-id"), "sess-2"),
            ],
            INIT_TOOLS,
        )
            .into_response(),
        ("notifications/initialized", _) => StatusCode::ACCEPTED.into_response(),
        ("tools/list", None) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read","description":"Reads"},{"description":"no name"}],"nextCursor":"p2"}}"#,
        )
            .into_response(),
        ("tools/list", Some("p2")) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(axum::body::Body::from(
                "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"tools\":[{\"name\":\"write\"}]}}\n\n",
            ))
            .unwrap(),
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

/// A server that declares tools but answers `tools/list` with an error that
/// echoes the `x-key` header.
async fn tools_error(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, &request);
    let key = request
        .headers()
        .get("x-key")
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let (method, _) = method_and_cursor(request).await;
    let body = match method.as_str() {
        "initialize" => INIT_TOOLS.to_owned(),
        "tools/list" => serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {"code": -32603, "message": format!("no tools for {key}")},
        })
        .to_string(),
        _ => return StatusCode::ACCEPTED.into_response(),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn start_fake() -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/mcp", any(mcp_json))
        .route("/sse", get(mcp_sse).post(mcp_sse))
        .route("/unauth", any(unauthorized))
        .route("/nonmcp", any(not_mcp))
        .route("/strict", any(strict))
        .route("/rpcerror", any(rpc_error))
        .route("/redirect", any(redirect))
        .route("/slow", any(slow))
        .route("/tools", any(with_tools))
        .route("/tools-error", any(tools_error))
        .with_state(Arc::clone(&fake));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (fake, format!("http://{addr}"))
}

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_json_reply_is_a_success_and_sends_the_headers_and_ends_the_session() {
    let (fake, base) = start_fake().await;
    let checker = HttpMcpChecker::new();
    let outcome = checker
        .check(
            &format!("{base}/mcp"),
            headers(&[("Authorization", "Bearer real-secret")]),
        )
        .await;
    assert_eq!(
        outcome,
        CheckOutcome::Ok {
            server_name: Some("fake".to_owned()),
            server_version: Some("9.9".to_owned()),
            protocol_version: "2025-06-18".to_owned(),
            tools: ToolList::Listed(Vec::new()),
        },
    );

    let seen = fake.seen();
    // The initialize POST carried the project's header, Accept and Content-Type.
    let init = &seen[0];
    assert_eq!(init.method, "POST");
    assert_eq!(init.header("authorization"), Some("Bearer real-secret"));
    assert_eq!(init.header("content-type"), Some("application/json"));
    assert!(
        init.header("accept").unwrap().contains("text/event-stream"),
        "the Accept header offers SSE",
    );
    // A best-effort DELETE ended the session the server opened.
    let delete = seen
        .iter()
        .find(|s| s.method == "DELETE")
        .expect("the session was ended with a DELETE");
    assert_eq!(delete.header("mcp-session-id"), Some("sess-1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sse_reply_is_a_success() {
    let (_fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(&format!("{base}/sse"), Vec::new())
        .await;
    assert_eq!(
        outcome,
        CheckOutcome::Ok {
            server_name: Some("fake".to_owned()),
            server_version: Some("9.9".to_owned()),
            protocol_version: "2025-06-18".to_owned(),
            tools: ToolList::Listed(Vec::new()),
        },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_is_refused() {
    let (_fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(&format!("{base}/unauth"), Vec::new())
        .await;
    assert_eq!(outcome, CheckOutcome::Refused { status: 401 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_mcp_2xx_body_is_not_an_mcp_reply() {
    let (_fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(&format!("{base}/nonmcp"), Vec::new())
        .await;
    assert_eq!(outcome, CheckOutcome::NotMcp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_initialize_request_carries_a_client_version() {
    let (_fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(&format!("{base}/strict"), Vec::new())
        .await;
    assert_eq!(
        outcome,
        CheckOutcome::Ok {
            server_name: Some("fake".to_owned()),
            server_version: Some("9.9".to_owned()),
            protocol_version: "2025-06-18".to_owned(),
            tools: ToolList::Listed(Vec::new()),
        },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_json_rpc_error_is_reported_cut_and_without_header_values() {
    let (_fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(
            &format!("{base}/rpcerror"),
            headers(&[("x-key", "sk-secret-123")]),
        )
        .await;
    let CheckOutcome::RpcError { code, message } = outcome else {
        panic!("expected a JSON-RPC error, got {outcome:?}");
    };
    assert_eq!(code, -32602);
    assert!(message.starts_with("bad key [redacted] x"), "{message}");
    assert!(!message.contains("sk-secret-123"));
    assert_eq!(
        message.chars().count(),
        303,
        "300 characters and an ellipsis"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_is_reported_and_not_followed() {
    let (fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(&format!("{base}/redirect"), Vec::new())
        .await;
    assert_eq!(
        outcome,
        CheckOutcome::HttpStatus {
            status: 302,
            location: Some("/elsewhere".to_owned()),
        },
    );
    // The checker did not follow the redirect to /elsewhere.
    assert!(
        fake.seen().iter().all(|s| s.path != "/elsewhere"),
        "the redirect must not be followed",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_port_is_unreachable() {
    // Bind then drop, so the port is free and a connect is refused at once.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", dead.local_addr().unwrap());
    drop(dead);
    let outcome = HttpMcpChecker::new().check(&url, Vec::new()).await;
    assert_eq!(outcome, CheckOutcome::Unreachable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_server_times_out() {
    let (_fake, base) = start_fake().await;
    // A short total timeout, so the test need not wait the production window.
    let checker = HttpMcpChecker::with_timeout(Duration::from_millis(300));
    let outcome = checker.check(&format!("{base}/slow"), Vec::new()).await;
    assert_eq!(outcome, CheckOutcome::Timeout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_check_lists_every_page_of_tools_in_the_session() {
    let (fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(&format!("{base}/tools"), headers(&[("x-key", "k")]))
        .await;
    assert_eq!(
        outcome,
        CheckOutcome::Ok {
            server_name: Some("fake".to_owned()),
            server_version: None,
            protocol_version: "2025-03-26".to_owned(),
            tools: ToolList::Listed(vec![
                McpTool {
                    name: "read".to_owned(),
                    description: Some("Reads".to_owned()),
                },
                McpTool {
                    name: "write".to_owned(),
                    description: None,
                },
            ]),
        },
    );
    // initialize, initialized, two pages, and the DELETE.
    let seen = fake.seen();
    assert_eq!(seen.len(), 5);
    for request in &seen[1..] {
        assert_eq!(request.header("mcp-session-id"), Some("sess-2"));
        assert_eq!(request.header("x-key"), Some("k"));
    }
    for request in &seen[1..4] {
        assert_eq!(request.header("mcp-protocol-version"), Some("2025-03-26"));
    }
    assert_eq!(seen[4].method, "DELETE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_without_the_tools_capability_lists_none() {
    let (fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(&format!("{base}/mcp"), Vec::new())
        .await;
    let CheckOutcome::Ok { tools, .. } = outcome else {
        panic!("expected a success, got {outcome:?}");
    };
    assert_eq!(tools, ToolList::Listed(Vec::new()));
    // Only initialize and the DELETE: no tools/list was sent.
    assert_eq!(fake.seen().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tools_list_error_fails_the_listing_without_header_values() {
    let (_fake, base) = start_fake().await;
    let outcome = HttpMcpChecker::new()
        .check(
            &format!("{base}/tools-error"),
            headers(&[("x-key", "sk-secret-123")]),
        )
        .await;
    let CheckOutcome::Ok {
        tools: ToolList::Failed(reason),
        ..
    } = outcome
    else {
        panic!("expected a failed listing, got {outcome:?}");
    };
    assert!(reason.contains("-32603"), "{reason}");
    assert!(reason.contains("[redacted]"), "{reason}");
    assert!(!reason.contains("sk-secret-123"), "{reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_https_server_passes_only_with_its_ca_as_an_extra_root() {
    let app = Router::new()
        .route("/mcp", any(mcp_json))
        .with_state(Arc::new(Fake::default()));
    let tls = support::serve_tls(app).await;
    let url = format!("{}/mcp", tls.base);

    let without = HttpMcpChecker::new().check(&url, Vec::new()).await;
    assert!(
        without == CheckOutcome::Unreachable,
        "an unknown CA is not trusted: {without:?}",
    );

    let roots = reqwest::Certificate::from_pem_bundle(tls.ca_pem.as_bytes()).unwrap();
    let with = HttpMcpChecker::with_roots(roots)
        .check(&url, Vec::new())
        .await;
    assert!(matches!(with, CheckOutcome::Ok { .. }), "{with:?}");
}
