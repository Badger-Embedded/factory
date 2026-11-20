//! Tests for the MCP proxy. A local fake MCP server stands in for a real one,
//! so the tests reach only `127.0.0.1` and open no outside connection. The
//! proxy connects to the fake over plain HTTP; production uses the same code
//! path against an `https` server.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use factory_server::{
    LocalSecrets, MASTER_KEY_LEN, ProxyWarning, RunTokens, SecretRef, SecretStore, Store,
    StoreUpstreams, Upstream, Upstreams, proxy_app,
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
}

impl Upstreams for FakeUpstreams {
    fn resolve(&self, _project: &str, server: &str) -> Option<Upstream> {
        self.known.contains(server).then(|| Upstream {
            url: self.url.clone(),
            headers: self.headers.clone(),
        })
    }
}

/// The proxy wired to `upstreams`, plus the token registry and the list the
/// warning callback appends to. Returns the base URL, the tokens and the
/// warnings.
async fn start_proxy(
    upstreams: Arc<dyn Upstreams>,
) -> (String, Arc<RunTokens>, Arc<Mutex<Vec<ProxyWarning>>>) {
    let tokens = Arc::new(RunTokens::new());
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&warnings);
    let warn = Arc::new(move |warning: ProxyWarning| sink.lock().unwrap().push(warning));
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
    assert_eq!(warnings[0].job, 9);
    assert_eq!(warnings[0].server, "context7");
    assert!(warnings[0].reason.contains("500"), "{}", warnings[0].reason);
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
    assert_eq!(warnings[0].job, 4);
    assert_eq!(warnings[0].server, "context7");
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
