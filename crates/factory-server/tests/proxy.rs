//! Tests for the MCP proxy. A local fake MCP server stands in for a real one,
//! so the tests reach only `127.0.0.1` and open no outside connection. The
//! proxy connects to the fake over plain HTTP; production uses the same code
//! path against an `https` server.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod support;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use factory_server::{
    LocalSecrets, MASTER_KEY_LEN, McpTool, ProxyEvent, ProxyEventFn, RunTokens, SecretRef,
    SecretStore, Store, StoreUpstreams, Upstream, Upstreams, proxy_app, proxy_app_with_roots,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// One request the fake MCP server received, so a test can check what reached
/// the server through the proxy.
#[derive(Clone)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// The fake MCP server's shared state: the requests it saw, whether the
/// redirect target was ever fetched, and the channel that drives a streaming
/// response.
#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<Seen>>,
    redirect_followed: Mutex<bool>,
    sse: Mutex<Option<mpsc::Receiver<Result<Bytes, std::io::Error>>>>,
}

impl Fake {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Records a request, then answers 200 with a session id and a short body, the
/// shape of a non-streaming MCP reply.
async fn record_and_reply(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, request).await;
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (HeaderName::from_static("mcp-session-id"), "server-session"),
        ],
        "{\"ok\":true}",
    )
        .into_response()
}

/// Streams a `text/event-stream` response from the channel in the fake's state,
/// so a test sends events one at a time and checks each arrives before the next
/// is sent.
async fn stream_events(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, request).await;
    let rx = fake
        .sse
        .lock()
        .unwrap()
        .take()
        .expect("a test set the SSE channel");
    let body = Body::from_stream(ReceiverStream::new(rx));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("mcp-session-id", "server-session")
        .body(body)
        .unwrap()
}

/// Answers a redirect that points back at the fake. If the proxy followed it,
/// the fake's `/target` handler would run and set the flag.
async fn redirect(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, request).await;
    (StatusCode::FOUND, [(header::LOCATION, "/target")], "moved").into_response()
}

async fn target(State(fake): State<Arc<Fake>>) -> Response {
    *fake.redirect_followed.lock().unwrap() = true;
    (StatusCode::OK, "target").into_response()
}

async fn server_error(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, request).await;
    (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response()
}

/// A `tools/list` result with three tools, for the request `id` 4.
const TOOLS_RESULT: &str = r#"{"jsonrpc":"2.0","id":4,"result":{"tools":[{"name":"read"},{"name":"delete"},{"name":"write"}],"nextCursor":"next"}}"#;

/// Answers any POST with [`TOOLS_RESULT`] as JSON.
async fn tools_json(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, request).await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        TOOLS_RESULT,
    )
        .into_response()
}

/// Answers any POST with an SSE stream: a notification event, then
/// [`TOOLS_RESULT`] split over two `data:` lines with CRLF line ends.
async fn tools_sse(State(fake): State<Arc<Fake>>, request: Request) -> Response {
    record(&fake, request).await;
    let (head, tail) = TOOLS_RESULT.split_at(TOOLS_RESULT.find("\"result\"").unwrap());
    let body = format!(
        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n\n\
         id: 7\r\nevent: message\r\ndata: {head}\r\ndata: {tail}\r\n\r\n"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

async fn record(fake: &Fake, request: Request) {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().map(str::to_owned);
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
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    fake.seen.lock().unwrap().push(Seen {
        method,
        path,
        query,
        headers,
        body,
    });
}

/// Starts the fake MCP server and returns its state and base URL.
async fn start_fake() -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/mcp", any(record_and_reply))
        .route("/sse", get(stream_events))
        .route("/redirect", any(redirect))
        .route("/target", get(target))
        .route("/error", any(server_error))
        .route("/tools", any(tools_json))
        .route("/tools-sse", any(tools_sse))
        .with_state(Arc::clone(&fake));
    let base = serve(app).await;
    (fake, base)
}

/// Serves `app` on an ephemeral port and returns its `http://host:port` base.
async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// A fake [`Upstreams`] that points a known server at a fake URL and adds a set
/// of headers, so the proxy's token and header handling is exercised without a
/// store.
struct FakeUpstreams {
    url: String,
    headers: Vec<(String, String)>,
    known: HashSet<String>,
    disabled: HashSet<String>,
}

impl Upstreams for FakeUpstreams {
    fn resolve(&self, _project: &str, server: &str) -> Option<Upstream> {
        self.known.contains(server).then(|| Upstream {
            url: self.url.clone(),
            headers: self.headers.clone(),
            disabled_tools: self.disabled.clone(),
        })
    }
}

/// The proxy wired to `upstreams`, plus the token registry and the list the
/// warning callback appends to. Returns the base URL, the tokens and the
/// warnings.
async fn start_proxy(
    upstreams: Arc<dyn Upstreams>,
) -> (String, Arc<RunTokens>, Arc<Mutex<Vec<ProxyEvent>>>) {
    let tokens = Arc::new(RunTokens::new());
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&warnings);
    let warn = Arc::new(move |warning: ProxyEvent| sink.lock().unwrap().push(warning));
    let app = proxy_app(Arc::clone(&tokens), upstreams, warn);
    let base = serve(app).await;
    (base, tokens, warnings)
}

fn fake_upstreams(url: &str, headers: &[(&str, &str)], known: &[&str]) -> Arc<dyn Upstreams> {
    Arc::new(FakeUpstreams {
        url: url.to_owned(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        known: known.iter().map(|name| (*name).to_owned()).collect(),
        disabled: HashSet::new(),
    })
}

/// A client that does not follow redirects, so a test observes a 3xx rather
/// than its target.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_the_projects_headers_and_not_the_run_token() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(
        &format!("{fake_base}/mcp"),
        &[
            ("Authorization", "Bearer real-secret"),
            ("X-Api-Key", "key-123"),
        ],
        &["context7"],
    );
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(7, "maestro", &["context7".to_owned()]);

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .header(header::CONTENT_TYPE, "application/json")
        .header("mcp-session-id", "client-session")
        .header("x-extra", "should-be-dropped")
        .body("{\"jsonrpc\":\"2.0\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = fake.seen();
    assert_eq!(seen.len(), 1, "the fake received exactly one request");
    let seen = &seen[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/mcp");
    assert_eq!(seen.body, b"{\"jsonrpc\":\"2.0\"}");
    // The stored credentials reach the server.
    assert_eq!(seen.header("authorization"), Some("Bearer real-secret"));
    assert_eq!(seen.header("x-api-key"), Some("key-123"));
    // The protocol headers are forwarded.
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("mcp-session-id"), Some("client-session"));
    // The run token never reaches the server, and no other worker header does.
    assert!(
        seen.header("x-extra").is_none(),
        "a non-protocol header is dropped"
    );
    for (_, value) in &seen.headers {
        assert!(
            !value.contains(token.as_str()),
            "the run token must not reach the server",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passes_the_session_id_back_to_the_worker() {
    let (_fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["context7".to_owned()]);

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response
            .headers()
            .get("mcp-session-id")
            .map(|v| v.to_str().unwrap()),
        Some("server-session"),
        "the server's session id reaches the worker",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supports_post_get_and_delete() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["context7".to_owned()]);
    let url = format!("{proxy}/mcp/context7");

    for method in [
        reqwest::Method::POST,
        reqwest::Method::GET,
        reqwest::Method::DELETE,
    ] {
        let response = client()
            .request(method.clone(), &url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{method} is proxied");
    }
    let methods: Vec<String> = fake.seen().into_iter().map(|s| s.method).collect();
    assert_eq!(methods, vec!["POST", "GET", "DELETE"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_the_query_string() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["context7".to_owned()]);

    client()
        .get(format!("{proxy}/mcp/context7?sessionId=abc"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(fake.seen()[0].query.as_deref(), Some("sessionId=abc"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_token_is_unauthorized() {
    let (_fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, _tokens, _warnings) = start_proxy(upstreams).await;

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_token_is_unauthorized() {
    let (_fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, _tokens, _warnings) = start_proxy(upstreams).await;

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth("not-a-real-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_token_is_unauthorized() {
    let (_fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["context7".to_owned()]);

    // Before the run ends the token works.
    let ok = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    // The worker exits and the factory revokes the token.
    tokens.revoke(&token);
    let after = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_outside_the_token_scope_is_forbidden() {
    let (_fake, fake_base) = start_fake().await;
    // The token covers `other`, but the request is for `context7`.
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7", "other"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["other".to_owned()]);

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_is_returned_and_not_followed() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/redirect"), &[], &["context7"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["context7".to_owned()]);

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap()),
        Some("/target"),
        "the redirect is relayed to the worker",
    );
    assert!(
        !*fake.redirect_followed.lock().unwrap(),
        "the proxy did not follow the redirect",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streaming_response_passes_event_by_event() {
    let (fake, fake_base) = start_fake().await;
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    *fake.sse.lock().unwrap() = Some(rx);
    let upstreams = fake_upstreams(&format!("{fake_base}/sse"), &[], &["context7"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["context7".to_owned()]);

    let mut response = client()
        .get(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap()),
        Some("text/event-stream"),
    );

    // Each event is read before the next is sent. If the proxy buffered the
    // body, the first read would block until the stream closed, which never
    // happens, so the test would time out.
    tx.send(Ok(Bytes::from_static(b"data: one\n\n")))
        .await
        .unwrap();
    let first = next_chunk(&mut response).await;
    assert_eq!(first, b"data: one\n\n");

    tx.send(Ok(Bytes::from_static(b"data: two\n\n")))
        .await
        .unwrap();
    let second = next_chunk(&mut response).await;
    assert_eq!(second, b"data: two\n\n");

    drop(tx);
}

/// The next body chunk, or a panic if none arrives within a short window, so a
/// buffering regression fails fast instead of hanging the suite.
async fn next_chunk(response: &mut reqwest::Response) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await
        .expect("a chunk arrives before the timeout")
        .unwrap()
        .expect("the stream has another chunk")
        .to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_error_is_relayed_and_warned() {
    let (_fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/error"), &[], &["context7"]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(9, "maestro", &["context7".to_owned()]);

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let warnings = warnings.lock().unwrap();
    assert_eq!(warnings.len(), 1);
    let ProxyEvent::Unavailable {
        job,
        server,
        reason,
    } = &warnings[0]
    else {
        panic!("expected an unavailable warning, got {:?}", warnings[0]);
    };
    assert_eq!(*job, 9);
    assert_eq!(server, "context7");
    assert!(reason.contains("500"), "{reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_server_is_warned() {
    // A port nothing listens on: binding then dropping frees it, so a connect
    // is refused at once.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!("http://{}/mcp", dead.local_addr().unwrap());
    drop(dead);
    let upstreams = fake_upstreams(&dead_url, &[], &["context7"]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(4, "maestro", &["context7".to_owned()]);

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let warnings = warnings.lock().unwrap();
    assert_eq!(warnings.len(), 1);
    let ProxyEvent::Unavailable { job, server, .. } = &warnings[0] else {
        panic!("expected an unavailable warning, got {:?}", warnings[0]);
    };
    assert_eq!(*job, 4);
    assert_eq!(server, "context7");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deregistered_server_is_warned() {
    let (_fake, fake_base) = start_fake().await;
    // The token covers `context7`, but the resolver no longer knows it, as if
    // it were deleted mid-run.
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &[]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(5, "maestro", &["context7".to_owned()]);

    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(warnings.lock().unwrap().len(), 1);
}

/// The production resolver reads the server's URL from the registry and the
/// header value from the encrypted secret store, with no network.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_upstreams_resolves_a_stored_header() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_mcp_server(
            "context7",
            "https://mcp.context7.com/mcp",
            &["Authorization".to_owned()],
        )
        .unwrap();
    let secrets = Arc::new(LocalSecrets::new(&[3; MASTER_KEY_LEN], Arc::clone(&store)));
    secrets
        .put(
            SecretRef {
                project: "maestro",
                server: "context7",
                header: "Authorization",
            },
            "Bearer sk-stored",
        )
        .unwrap();

    let upstreams = StoreUpstreams::new(Arc::clone(&store), secrets);
    let resolved = upstreams.resolve("maestro", "context7").expect("resolves");
    assert_eq!(resolved.url, "https://mcp.context7.com/mcp");
    assert_eq!(
        resolved.headers,
        vec![("Authorization".to_owned(), "Bearer sk-stored".to_owned())],
    );
    // A project with no stored value gets the server with no header.
    let keyless = upstreams.resolve("other", "context7").expect("resolves");
    assert_eq!(keyless.headers, Vec::<(String, String)>::new());
    // An unregistered server does not resolve.
    assert!(upstreams.resolve("maestro", "absent").is_none());
}

#[test]
fn a_stdio_server_does_not_resolve_through_the_proxy() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_mcp_server_with(
            "playwright",
            &factory_server::McpTransport::Stdio {
                command: "npx".to_owned(),
                args: Vec::new(),
            },
        )
        .unwrap();
    let secrets = Arc::new(LocalSecrets::new(&[3; MASTER_KEY_LEN], Arc::clone(&store)));
    let upstreams = StoreUpstreams::new(store, secrets);
    assert!(upstreams.resolve("maestro", "playwright").is_none());
}

/// The proxy in front of the fake at `path`, with `delete` turned off for the
/// project. Returns the proxy base, a token for job 3 and the warnings.
async fn proxy_with_delete_off(
    path: &str,
) -> (Arc<Fake>, String, String, Arc<Mutex<Vec<ProxyEvent>>>) {
    let (fake, fake_base) = start_fake().await;
    let upstreams: Arc<dyn Upstreams> = Arc::new(FakeUpstreams {
        url: format!("{fake_base}{path}"),
        headers: Vec::new(),
        known: HashSet::from(["context7".to_owned()]),
        disabled: HashSet::from(["delete".to_owned()]),
    });
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(3, "maestro", &["context7".to_owned()]);
    (fake, proxy, token, warnings)
}

async fn post_rpc(proxy: &str, token: &str, body: &str) -> reqwest::Response {
    client()
        .post(format!("{proxy}/mcp/context7"))
        .bearer_auth(token)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_to_a_turned_off_tool_is_answered_by_the_proxy() {
    let (fake, proxy, token, warnings) = proxy_with_delete_off("/mcp").await;

    let response = post_rpc(
        &proxy,
        &token,
        r#"{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"delete","arguments":{}}}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(reply["id"], "call-1");
    assert_eq!(reply["error"]["code"], -32602);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("delete is turned off"),
    );
    assert!(fake.seen().is_empty(), "the call reached the server");
    assert_eq!(
        *warnings.lock().unwrap(),
        vec![ProxyEvent::ToolBlocked {
            job: 3,
            server: "context7".to_owned(),
            tool: "delete".to_owned(),
        }],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_to_a_tool_that_is_on_is_forwarded() {
    let (fake, proxy, token, warnings) = proxy_with_delete_off("/mcp").await;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read"}}"#;
    let response = post_rpc(&proxy, &token, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.seen()[0].body, body.as_bytes());
    // The call is forwarded, so it is recorded. With `delete` off, a call to the
    // `read` tool that is on still reports one forwarded call and nothing else.
    assert_eq!(
        *warnings.lock().unwrap(),
        vec![ProxyEvent::ToolCalled {
            job: 3,
            server: "context7".to_owned(),
            tool: "read".to_owned(),
        }],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forwarded_call_is_recorded_with_no_tool_off() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(7, "maestro", &["context7".to_owned()]);
    let body =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"resolve-library-id"}}"#;
    let response = post_rpc(&proxy, &token, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    // With no tool off the body still reaches the server unchanged.
    assert_eq!(fake.seen()[0].body, body.as_bytes());
    assert_eq!(
        *warnings.lock().unwrap(),
        vec![ProxyEvent::ToolCalled {
            job: 7,
            server: "context7".to_owned(),
            tool: "resolve-library-id".to_owned(),
        }],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_records_a_forwarded_call_for_each_tool() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(7, "maestro", &["context7".to_owned()]);
    // Two calls and a list in one batch report two forwarded calls, in order.
    let body = r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"resolve-library-id"}},
        {"jsonrpc":"2.0","id":2,"method":"tools/list"},
        {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get-library-docs"}}]"#;
    let response = post_rpc(&proxy, &token, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.seen()[0].body, body.as_bytes());
    assert_eq!(
        *warnings.lock().unwrap(),
        vec![
            ProxyEvent::ToolCalled {
                job: 7,
                server: "context7".to_owned(),
                tool: "resolve-library-id".to_owned(),
            },
            ProxyEvent::ToolCalled {
                job: 7,
                server: "context7".to_owned(),
                tool: "get-library-docs".to_owned(),
            },
        ],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_call_bodies_record_nothing() {
    let (_fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(7, "maestro", &["context7".to_owned()]);
    for body in [
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        "",
    ] {
        let response = post_rpc(&proxy, &token, body).await;
        assert_eq!(response.status(), StatusCode::OK, "{body:?}");
    }
    assert!(
        warnings.lock().unwrap().is_empty(),
        "a list, an initialize and an empty body record nothing",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_json_body_records_nothing_with_no_tool_off() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(7, "maestro", &["context7".to_owned()]);
    let response = post_rpc(&proxy, &token, "not json").await;
    assert_eq!(response.status(), StatusCode::OK);
    // The body is forwarded as before and nothing is recorded.
    assert_eq!(fake.seen()[0].body, b"not json");
    assert!(warnings.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_without_a_token_records_nothing() {
    let (_fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, warnings) = start_proxy(upstreams).await;
    let valid = tokens.issue(7, "maestro", &["context7".to_owned()]);
    let body =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"resolve-library-id"}}"#;

    // A missing token.
    let response = client()
        .post(format!("{proxy}/mcp/context7"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A revoked token.
    tokens.revoke(&valid);
    let response = post_rpc(&proxy, &valid, body).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    assert!(
        warnings.lock().unwrap().is_empty(),
        "an unauthorized request never reaches the call record",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_holding_a_turned_off_call_is_refused() {
    let (fake, proxy, token, warnings) = proxy_with_delete_off("/mcp").await;
    let response = post_rpc(
        &proxy,
        &token,
        r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read"}},
            {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delete"}}]"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(fake.seen().is_empty());
    assert_eq!(warnings.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_the_proxy_cannot_check_is_refused() {
    let (fake, proxy, token, _warnings) = proxy_with_delete_off("/mcp").await;
    for body in [
        // Not JSON.
        "not json",
        // A duplicate key: a server reading the first `name` would call `delete`.
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"delete","name":"read"}}"#,
        // A duplicate `method`.
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","method":"ping","params":{"name":"delete"}}"#,
        // A call with no string tool name.
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":7}}"#,
        // A method that is not a string.
        r#"{"jsonrpc":"2.0","id":1,"method":["tools/call"]}"#,
    ] {
        let response = post_rpc(&proxy, &token, body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(fake.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_tool_off_the_body_is_forwarded_unread() {
    let (fake, fake_base) = start_fake().await;
    let upstreams = fake_upstreams(&format!("{fake_base}/mcp"), &[], &["context7"]);
    let (proxy, tokens, _warnings) = start_proxy(upstreams).await;
    let token = tokens.issue(1, "maestro", &["context7".to_owned()]);
    let response = post_rpc(&proxy, &token, "not json").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.seen()[0].body, b"not json");
}

fn tool_names(reply: &serde_json::Value) -> Vec<&str> {
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

const LIST: &str = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_json_tools_list_reply_leaves_out_the_turned_off_tools() {
    let (_fake, proxy, token, _warnings) = proxy_with_delete_off("/tools").await;
    let response = post_rpc(&proxy, &token, LIST).await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(tool_names(&reply), vec!["read", "write"]);
    assert_eq!(reply["result"]["nextCursor"], "next");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tools_list_reply_to_another_id_is_relayed_unchanged() {
    let (_fake, proxy, token, _warnings) = proxy_with_delete_off("/tools").await;
    let response = post_rpc(
        &proxy,
        &token,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/list"}"#,
    )
    .await;
    assert_eq!(response.text().await.unwrap(), TOOLS_RESULT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sse_tools_list_reply_leaves_out_the_turned_off_tools() {
    let (_fake, proxy, token, _warnings) = proxy_with_delete_off("/tools-sse").await;
    let response = post_rpc(&proxy, &token, LIST).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = response.text().await.unwrap();
    let mut events = body.split("\n\n").filter(|event| !event.is_empty());
    // The notification passes as it was.
    assert_eq!(
        events.next().unwrap(),
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}",
    );
    // The response keeps its other lines and carries the filtered list.
    let response = events.next().unwrap();
    let mut lines = response.lines();
    assert_eq!(lines.next(), Some("id: 7"));
    assert_eq!(lines.next(), Some("event: message"));
    let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
    let reply: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(tool_names(&reply), vec!["read", "write"]);
    assert!(events.next().is_none());
}

/// The production resolver returns the tools the project turned off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_upstreams_returns_the_turned_off_tools() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    let tools: Vec<McpTool> = ["read", "delete"]
        .into_iter()
        .map(|name| McpTool {
            name: name.to_owned(),
            description: None,
        })
        .collect();
    store
        .replace_mcp_tools("maestro", "context7", &tools)
        .unwrap();
    store
        .save_mcp_tool_states("maestro", "context7", &[("delete".to_owned(), true)])
        .unwrap();
    let secrets = Arc::new(LocalSecrets::new(&[3; MASTER_KEY_LEN], Arc::clone(&store)));

    let upstreams = StoreUpstreams::new(Arc::clone(&store), secrets);
    let resolved = upstreams.resolve("maestro", "context7").unwrap();
    assert_eq!(
        resolved.disabled_tools,
        HashSet::from(["delete".to_owned()])
    );
    // Another project has every tool on.
    assert!(
        upstreams
            .resolve("other", "context7")
            .unwrap()
            .disabled_tools
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxies_to_an_https_upstream_only_with_its_ca_as_an_extra_root() {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/mcp", any(record_and_reply))
        .with_state(Arc::clone(&fake));
    let tls = support::serve_tls(app).await;
    let roots = reqwest::Certificate::from_pem_bundle(tls.ca_pem.as_bytes()).unwrap();

    for (roots, expected) in [
        (Vec::new(), StatusCode::BAD_GATEWAY),
        (roots, StatusCode::OK),
    ] {
        let tokens = Arc::new(RunTokens::new());
        let warn: ProxyEventFn = Arc::new(|_| {});
        let upstreams = fake_upstreams(&format!("{}/mcp", tls.base), &[], &["context7"]);
        let app = proxy_app_with_roots(Arc::clone(&tokens), upstreams, warn, roots);
        let proxy = serve(app).await;
        let token = tokens.issue(7, "maestro", &["context7".to_owned()]);
        let response = client()
            .post(format!("{proxy}/mcp/context7"))
            .bearer_auth(&token)
            .header(header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}
