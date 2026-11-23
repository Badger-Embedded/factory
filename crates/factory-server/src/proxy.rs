//! The MCP proxy: the factory's own HTTP listener that stands between a worker
//! and a remote MCP server.
//!
//! Workers run with `bypassPermissions`, so a credential handed to one through
//! its environment, arguments or mounts could be read or exfiltrated by the
//! agent. The proxy keeps the credential out of the worker. Each agent run with
//! an active server gets a per-run token ([`RunTokens`]): 256 random bits held
//! only in memory, scoped to a job and the servers active for the run's stage.
//! The worker's MCP config points at `http://<proxy>/mcp/<server>` and carries
//! `Authorization: Bearer <token>` as its only header. The proxy checks the
//! token, swaps that header for the project's stored headers, and forwards the
//! request to the server. A factory restart drops every token, and the factory
//! revokes a run's token when its worker exits.
//!
//! The upstream URL and the stored headers come through the [`Upstreams`]
//! trait, so the proxy can read the registry and the encrypted secret store in
//! production and a fake in tests. A proxy-side failure to reach a server, a
//! connect timeout or a 5xx answer goes to a [`ProxyEventFn`] callback, which
//! records the server as unavailable for the run. The same callback records
//! each tool call the proxy forwards, so the job page can list them.
//!
//! A project can turn off single tools of a server. The proxy enforces it,
//! because the worker could send a `tools/call` with its run token from a shell
//! and skip any client-side filter. When the server has a tool turned off, the
//! proxy parses each request body as JSON-RPC and answers a `tools/call` of a
//! turned-off tool itself, without forwarding it. It also removes the turned-off
//! tools from the server's `tools/list` reply, so the model does not see them.
//! A refused call goes to the [`ProxyEventFn`] as well, and is never also
//! recorded as a forwarded call.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::mcp::McpTransport;
use crate::secrets::{SecretRef, SecretStore};
use crate::store::Store;

/// A run token's length in bytes. 32 bytes is 256 bits, so a token cannot be
/// guessed and two never collide.
const TOKEN_BYTES: usize = 32;

/// How long the proxy waits for a connection to a server before it gives up.
/// There is no total timeout: a streaming `text/event-stream` response may stay
/// open for the length of a run.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest request body the proxy buffers before forwarding it. MCP
/// requests are small JSON-RPC messages; the response, which may stream, is
/// never buffered.
const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;

/// The most of a `tools/list` reply the proxy buffers to remove turned-off
/// tools: the whole body for a JSON reply, one event for an SSE reply. A reply
/// over this is relayed unchanged; the call block still applies to it.
const MAX_TOOLS_LIST: usize = 4 * 1024 * 1024;

/// The JSON-RPC error code the proxy answers a turned-off tool's call with,
/// the "invalid params" code the MCP spec uses for an unknown tool.
const TOOL_OFF_CODE: i64 = -32602;

/// The request headers the proxy forwards to a server. The streamable-HTTP
/// transport owns these; everything else the worker sends, including its
/// `Authorization` run token, is dropped. Lowercase, as [`HeaderMap`] stores
/// names.
const FORWARD_HEADERS: [&str; 5] = [
    "content-type",
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// Response headers the proxy does not relay back to the worker. These are
/// hop-by-hop or framing headers that belong to this connection, so the
/// factory's own HTTP stack sets them for the leg to the worker. Every other
/// header, such as `Mcp-Session-Id` and a 3xx `Location`, passes through.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// What a run token grants: the job it belongs to, the project whose stored
/// headers the proxy adds, and the servers active for the run's stage. A
/// request for a server outside this set is refused.
#[derive(Debug, Clone)]
struct TokenScope {
    job: i64,
    project: String,
    servers: HashSet<String>,
}

/// The per-run tokens the proxy honours, held only in memory. The factory
/// issues a token when a run with an active server starts and revokes it when
/// the worker exits. A restart drops the map, so every token from before it is
/// invalid.
pub struct RunTokens {
    issued: Mutex<HashMap<String, TokenScope>>,
}

impl RunTokens {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        RunTokens {
            issued: Mutex::new(HashMap::new()),
        }
    }

    /// Issues a fresh token scoped to `job`, `project` and `servers`, and
    /// returns it. The caller passes the token to the worker and revokes it when
    /// the worker exits.
    #[must_use]
    pub fn issue(&self, job: i64, project: &str, servers: &[String]) -> String {
        let token = random_token();
        let scope = TokenScope {
            job,
            project: project.to_owned(),
            servers: servers.iter().cloned().collect(),
        };
        self.lock().insert(token.clone(), scope);
        token
    }

    /// Revokes `token`, so a later request with it is refused. Revoking a token
    /// that is not there, such as a run with no active server, is fine.
    pub fn revoke(&self, token: &str) {
        self.lock().remove(token);
    }

    /// Whether `token` is still issued. The proxy answers 401 to any token this
    /// returns `false` for, so a run's token is live only between its issue and
    /// the revoke when the worker exits.
    #[must_use]
    pub fn is_active(&self, token: &str) -> bool {
        self.lock().contains_key(token)
    }

    fn lookup(&self, token: &str) -> Option<TokenScope> {
        self.lock().get(token).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TokenScope>> {
        // A panic while a token is held would poison the lock. Recover the map
        // rather than fail every later request, since no invariant spans the
        // guard.
        self.issued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for RunTokens {
    fn default() -> Self {
        RunTokens::new()
    }
}

/// The upstream an in-scope server proxies to: its base URL and the header
/// name/value pairs the proxy adds to each forwarded request. The values are
/// the project's credentials, so they are built per request and never stored
/// on the proxy.
pub struct Upstream {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// The tools the project turned off. The proxy refuses a call to any of
    /// them and leaves them out of a `tools/list` reply.
    pub disabled_tools: HashSet<String>,
}

/// Resolves an in-scope server name to its [`Upstream`] for a project. The
/// proxy holds a `&dyn Upstreams`, so production reads the registry and the
/// encrypted secret store while a test points at a local fake server.
pub trait Upstreams: Send + Sync {
    /// The upstream for `project`'s use of `server`, or `None` when no server
    /// is registered under that name, such as one deleted while a run holds a
    /// token for it.
    fn resolve(&self, project: &str, server: &str) -> Option<Upstream>;
}

/// The production [`Upstreams`]: the server's URL comes from the registry,
/// each stored header value from the encrypted secret store, and the turned-off
/// tools from the project's tool settings. A header with no stored value, or
/// one that does not decrypt, is left off, so it is never forwarded. When the
/// turned-off tools cannot be read, the server resolves to `None`, so no call
/// reaches it unchecked.
pub struct StoreUpstreams {
    store: Arc<Store>,
    secrets: Arc<dyn SecretStore>,
}

impl StoreUpstreams {
    /// Builds the resolver from the registry store and the secret store.
    #[must_use]
    pub fn new(store: Arc<Store>, secrets: Arc<dyn SecretStore>) -> Self {
        StoreUpstreams { store, secrets }
    }
}

impl Upstreams for StoreUpstreams {
    fn resolve(&self, project: &str, server: &str) -> Option<Upstream> {
        let registered = match self.store.mcp_server(server) {
            Ok(Some(server)) => server,
            Ok(None) => return None,
            Err(err) => {
                tracing::error!("the MCP proxy could not read server {server:?}: {err}");
                return None;
            }
        };
        let McpTransport::Remote {
            url,
            headers: declared,
        } = registered.transport;
        let mut headers = Vec::new();
        for header in &declared {
            let at = SecretRef {
                project,
                server,
                header,
            };
            match self.secrets.get(at) {
                Ok(Some(value)) => headers.push((header.clone(), value)),
                Ok(None) => {}
                // The error names no value, so it leaks no credential. A value
                // that cannot be read is left off, the same as an unset one.
                Err(err) => {
                    tracing::warn!(
                        "the MCP proxy could not read a stored header for {server:?}: {err}"
                    );
                }
            }
        }
        let disabled_tools = match self.store.disabled_mcp_tools(project, server) {
            Ok(tools) => tools,
            Err(err) => {
                tracing::error!("the MCP proxy could not read the tools {server:?} has off: {err}");
                return None;
            }
        };
        Some(Upstream {
            url,
            headers,
            disabled_tools,
        })
    }
}

/// Something the proxy reports about a run, which the factory records on the
/// job. No variant carries a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyEvent {
    /// The server was unavailable for the run, with a short reason.
    Unavailable {
        job: i64,
        server: String,
        reason: String,
    },
    /// The proxy refused a call to a tool the project turned off.
    ToolBlocked {
        job: i64,
        server: String,
        tool: String,
    },
    /// The proxy forwarded a call to a tool. One event per `tools/call` it
    /// lets through, whether or not the project turned any tool off.
    ToolCalled {
        job: i64,
        server: String,
        tool: String,
    },
}

/// The callback the proxy calls when it cannot reach a server, a connection
/// times out, a server answers 5xx, it refuses a turned-off tool's call, or it
/// forwards a tool call.
pub type ProxyEventFn = Arc<dyn Fn(ProxyEvent) + Send + Sync>;

#[derive(Clone)]
struct ProxyState {
    tokens: Arc<RunTokens>,
    upstreams: Arc<dyn Upstreams>,
    client: reqwest::Client,
    report: ProxyEventFn,
}

/// The proxy router. It listens on the factory's MCP port, separate from the
/// dashboard, and serves `/mcp/<server>` with any method the streamable-HTTP
/// transport uses. `tokens` gates each request, `upstreams` resolves the
/// server, and `report` records a server the proxy could not reach, a call it
/// refused, or a call it forwarded.
///
/// # Panics
///
/// Panics if the HTTP client cannot be built, which needs no network and
/// happens only on a broken TLS backend.
pub fn proxy_app(
    tokens: Arc<RunTokens>,
    upstreams: Arc<dyn Upstreams>,
    report: ProxyEventFn,
) -> Router {
    proxy_app_with_roots(tokens, upstreams, report, Vec::new())
}

/// As [`proxy_app`], with `roots` trusted next to the built-in roots, for an
/// upstream whose certificate is not from a public CA.
///
/// # Panics
///
/// As [`proxy_app`].
pub fn proxy_app_with_roots(
    tokens: Arc<RunTokens>,
    upstreams: Arc<dyn Upstreams>,
    report: ProxyEventFn,
    roots: Vec<reqwest::Certificate>,
) -> Router {
    // No redirect is followed, so a 3xx cannot carry the project's headers to
    // another host; it is returned to the worker as-is. A connect timeout
    // bounds the wait to reach a server; there is no total timeout, so a
    // streaming response may stay open as long as the run.
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT);
    for root in roots {
        builder = builder.add_root_certificate(root);
    }
    let client = builder.build().expect("build the MCP proxy HTTP client");
    let state = ProxyState {
        tokens,
        upstreams,
        client,
        report,
    };
    Router::new()
        .route("/mcp/{server}", any(proxy))
        .route("/mcp/{server}/{*suffix}", any(proxy_suffix))
        .with_state(state)
}

async fn proxy(
    State(state): State<ProxyState>,
    Path(server): Path<String>,
    request: Request,
) -> Response {
    forward(state, server, String::new(), request).await
}

async fn proxy_suffix(
    State(state): State<ProxyState>,
    Path((server, suffix)): Path<(String, String)>,
    request: Request,
) -> Response {
    forward(state, server, format!("/{suffix}"), request).await
}

async fn forward(state: ProxyState, server: String, suffix: String, request: Request) -> Response {
    let scope = match authorize(&state.tokens, request.headers(), &server) {
        Ok(scope) => scope,
        Err((status, message)) => return (status, message).into_response(),
    };
    let Some(upstream) = state.upstreams.resolve(&scope.project, &server) else {
        (state.report)(ProxyEvent::Unavailable {
            job: scope.job,
            server,
            reason: "the server is no longer registered".to_owned(),
        });
        return unreachable_response();
    };

    let method = request.method().clone();
    let query = request.uri().query().map(str::to_owned);
    let mut headers = protocol_headers(request.headers());
    // The stored header values are the credential. They replace any header of
    // the same name the worker sent, and the worker's run token is already
    // gone, since `protocol_headers` never copies `Authorization`.
    for (name, value) in &upstream.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_REQUEST_BODY).await else {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "the MCP request body is too large",
        )
            .into_response();
    };

    let list_id = match screen(&state, scope.job, &server, &body, &upstream.disabled_tools) {
        Ok(list_id) => list_id,
        Err(response) => return *response,
    };

    // The request is going upstream, so record every tool it calls. `screen`
    // already refused and reported any call to a turned-off tool, so a call
    // reported here is one the proxy forwards. The reading is lenient: a body
    // that is not JSON, or a `tools/call` with no string name, records nothing,
    // and the request is forwarded as before.
    for tool in tool_calls(&body) {
        (state.report)(ProxyEvent::ToolCalled {
            job: scope.job,
            server: server.clone(),
            tool,
        });
    }

    let mut url = upstream.url;
    url.push_str(&suffix);
    if let Some(query) = query {
        url.push('?');
        url.push_str(&query);
    }

    match state
        .client
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_server_error() {
                (state.report)(ProxyEvent::Unavailable {
                    job: scope.job,
                    server,
                    reason: format!("the server answered {}", response.status()),
                });
            }
            match list_id {
                Some(id) if response.status().is_success() => {
                    relay_tools_list(response, id, upstream.disabled_tools)
                }
                _ => relay(response),
            }
        }
        Err(err) => {
            let reason = if err.is_timeout() {
                "the connection to the server timed out"
            } else if err.is_connect() {
                "the server could not be reached"
            } else {
                "the request to the server failed"
            };
            (state.report)(ProxyEvent::Unavailable {
                job: scope.job,
                server,
                reason: reason.to_owned(),
            });
            unreachable_response()
        }
    }
}

/// Checks `body` against the project's turned-off tools. Returns the request's
/// `id` when it is a `tools/list` whose reply needs filtering, or the response
/// the proxy answers with instead of forwarding. With no tool turned off,
/// every body passes unread.
fn screen(
    state: &ProxyState,
    job: i64,
    server: &str,
    body: &[u8],
    disabled: &HashSet<String>,
) -> Result<Option<Value>, Box<Response>> {
    if disabled.is_empty() {
        return Ok(None);
    }
    match inspect(body, disabled) {
        Inspection::Forward { list_id } => Ok(list_id),
        Inspection::Blocked { tool, id } => {
            let response = tool_off_response(&tool, &id);
            (state.report)(ProxyEvent::ToolBlocked {
                job,
                server: server.to_owned(),
                tool,
            });
            Err(Box::new(response))
        }
        Inspection::BlockedBatch { tool } => {
            (state.report)(ProxyEvent::ToolBlocked {
                job,
                server: server.to_owned(),
                tool,
            });
            Err(Box::new(
                (
                    StatusCode::FORBIDDEN,
                    "the batch calls a tool this project turned off",
                )
                    .into_response(),
            ))
        }
        Inspection::Malformed => Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                "the request body is not a JSON-RPC message the proxy can check",
            )
                .into_response(),
        )),
    }
}

/// Checks the run token and that the server is in its scope. Returns the scope,
/// or the status and message to answer with: 401 for a missing, unknown or
/// revoked token, 403 for a server outside the token's scope.
fn authorize(
    tokens: &RunTokens,
    headers: &HeaderMap,
    server: &str,
) -> Result<TokenScope, (StatusCode, &'static str)> {
    const UNAUTHORIZED: (StatusCode, &str) =
        (StatusCode::UNAUTHORIZED, "a valid run token is required");
    let Some(token) = bearer(headers) else {
        return Err(UNAUTHORIZED);
    };
    let Some(scope) = tokens.lookup(&token) else {
        return Err(UNAUTHORIZED);
    };
    if !scope.servers.contains(server) {
        return Err((
            StatusCode::FORBIDDEN,
            "this run token is not scoped to that server",
        ));
    }
    Ok(scope)
}

/// The answer to the worker when a server cannot be reached. Claude Code then
/// carries on without the server's tools, as it does for any unreachable MCP
/// server.
fn unreachable_response() -> Response {
    (StatusCode::BAD_GATEWAY, "the MCP server is unreachable").into_response()
}

/// The bearer token from an `Authorization` header, or `None` when the header
/// is absent, not `Bearer`, or empty.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// The streamable-HTTP headers the worker sent, ready to forward. The worker's
/// `Authorization` run token is not among them, so it never reaches the server.
fn protocol_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in FORWARD_HEADERS {
        if let Some(value) = incoming.get(name)
            && let Ok(name) = HeaderName::from_bytes(name.as_bytes())
        {
            out.insert(name, value.clone());
        }
    }
    out
}

/// Turns the server's response into the worker's, streaming the body so a
/// `text/event-stream` passes event by event without buffering. Hop-by-hop and
/// framing headers are dropped; every other header, including `Mcp-Session-Id`,
/// is relayed.
fn relay(response: reqwest::Response) -> Response {
    let mut builder = Response::builder().status(response.status());
    if let Some(headers) = builder.headers_mut() {
        for (name, value) in response.headers() {
            if !is_hop_by_hop(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| unreachable_response())
}

/// What the proxy does with a request body when the server has a tool turned
/// off.
enum Inspection {
    /// Forward it. `list_id` is the request's `id` when it is a `tools/list`,
    /// whose reply the proxy filters.
    Forward { list_id: Option<Value> },
    /// A `tools/call` of the turned-off `tool`, answered with an error for `id`.
    Blocked { tool: String, id: Value },
    /// A batch with a `tools/call` of the turned-off `tool`.
    BlockedBatch { tool: String },
    /// A body that is not JSON, has a duplicate key, or holds a `tools/call`
    /// with no string tool name. The proxy cannot tell which tool it calls.
    Malformed,
}

/// Decides what to do with `body` given the project's turned-off tools.
///
/// A JSON object with the same key twice is malformed, because the proxy and
/// the server could each read a different one and so a different tool name.
fn inspect(body: &[u8], disabled: &HashSet<String>) -> Inspection {
    if body.is_empty() {
        return Inspection::Forward { list_id: None };
    }
    if serde_json::from_slice::<UniqueKeys>(body).is_err() {
        return Inspection::Malformed;
    }
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return Inspection::Malformed;
    };
    match &message {
        Value::Array(batch) => {
            for item in batch {
                match called_tool(item) {
                    Err(()) => return Inspection::Malformed,
                    Ok(Some(tool)) if disabled.contains(&tool) => {
                        return Inspection::BlockedBatch { tool };
                    }
                    Ok(_) => {}
                }
            }
            Inspection::Forward { list_id: None }
        }
        Value::Object(_) => match called_tool(&message) {
            Err(()) => Inspection::Malformed,
            Ok(Some(tool)) if disabled.contains(&tool) => Inspection::Blocked {
                tool,
                id: message.get("id").cloned().unwrap_or(Value::Null),
            },
            Ok(_) => {
                let is_list = message.get("method").and_then(Value::as_str) == Some("tools/list");
                Inspection::Forward {
                    list_id: is_list.then(|| message.get("id").cloned().unwrap_or(Value::Null)),
                }
            }
        },
        _ => Inspection::Malformed,
    }
}

/// Every tool a request body calls, for the forwarded-call record. A single
/// JSON-RPC object holds one `tools/call`; a batch array holds several. The
/// reading is lenient, so it is safe to run on a body `screen` passed without
/// reading: a body that is not JSON, and a `tools/call` with no string tool
/// name, yield nothing, and the request is forwarded exactly as before.
fn tool_calls(body: &[u8]) -> Vec<String> {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    match &message {
        Value::Array(batch) => batch
            .iter()
            .filter_map(|item| called_tool(item).ok().flatten())
            .collect(),
        Value::Object(_) => called_tool(&message).ok().flatten().into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The tool a JSON-RPC message calls: `Some` for a `tools/call`, `None` for any
/// other message, and an error for a `method` that is not a string or a
/// `tools/call` with no string `params.name`.
fn called_tool(message: &Value) -> Result<Option<String>, ()> {
    let Some(method) = message.get("method") else {
        return Ok(None);
    };
    if method.as_str().ok_or(())? != "tools/call" {
        return Ok(None);
    }
    message
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .map(|name| Some(name.to_owned()))
        .ok_or(())
}

/// A JSON value that fails to deserialize when any object in it has the same
/// key twice. `serde_json::Value` keeps the last of two equal keys silently.
struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueKeysVisitor)
    }
}

struct UniqueKeysVisitor;

impl<'de> Visitor<'de> for UniqueKeysVisitor {
    type Value = UniqueKeys;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value with no duplicate object key")
    }

    fn visit_bool<E>(self, _: bool) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_i64<E>(self, _: i64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_u64<E>(self, _: u64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_f64<E>(self, _: f64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_str<E>(self, _: &str) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_unit<E>(self) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<UniqueKeys, A::Error> {
        while seq.next_element::<UniqueKeys>()?.is_some() {}
        Ok(UniqueKeys)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UniqueKeys, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate object key"));
            }
            map.next_value::<UniqueKeys>()?;
        }
        Ok(UniqueKeys)
    }
}

/// The proxy's own answer to a `tools/call` of a turned-off tool: a JSON-RPC
/// error for the request's `id`, so the model reads why the call failed.
fn tool_off_response(tool: &str, id: &Value) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": TOOL_OFF_CODE,
            "message": format!("the tool {tool} is turned off for this project"),
        },
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Relays a `tools/list` reply with the turned-off tools removed from the
/// response to the request `id`. A JSON reply is buffered whole; an SSE reply
/// is rewritten one event at a time, so it still streams. A part over
/// [`MAX_TOOLS_LIST`] or one that does not parse passes unchanged.
fn relay_tools_list(response: reqwest::Response, id: Value, disabled: HashSet<String>) -> Response {
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let mut builder = Response::builder().status(response.status());
    if let Some(headers) = builder.headers_mut() {
        for (name, value) in response.headers() {
            if !is_hop_by_hop(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
    }
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let filter = ToolFilter { id, disabled };
        if is_sse {
            filter.sse(response, tx).await;
        } else {
            filter.json(response, tx).await;
        }
    });
    builder
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap_or_else(|_| unreachable_response())
}

type Chunks = mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>;

/// Removes turned-off tools from the reply to one `tools/list` request.
struct ToolFilter {
    id: Value,
    disabled: HashSet<String>,
}

impl ToolFilter {
    /// Buffers a JSON reply up to [`MAX_TOOLS_LIST`] and sends it filtered. A
    /// longer reply is sent unchanged as it arrives.
    async fn json(&self, mut response: reqwest::Response, tx: Chunks) {
        let mut buffer = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                    if buffer.len() > MAX_TOOLS_LIST {
                        if tx.send(Ok(buffer.into())).await.is_ok() {
                            pass_through(response, tx).await;
                        }
                        return;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let _ = tx.send(Err(std::io::Error::other(err))).await;
                    return;
                }
            }
        }
        let body = self.rewrite(&buffer).unwrap_or(buffer);
        let _ = tx.send(Ok(body.into())).await;
    }

    /// Sends an SSE reply event by event, each filtered once its blank-line
    /// end arrives. When a single event grows past [`MAX_TOOLS_LIST`], the rest
    /// of the stream is sent unchanged.
    async fn sse(&self, mut response: reqwest::Response, tx: Chunks) {
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    pending.extend_from_slice(&chunk);
                    while let Some(end) = event_end(&pending) {
                        let event: Vec<u8> = pending.drain(..end).collect();
                        let event = self.rewrite_event(&event).unwrap_or(event);
                        if tx.send(Ok(event.into())).await.is_err() {
                            return;
                        }
                    }
                    if pending.len() > MAX_TOOLS_LIST {
                        if tx
                            .send(Ok(std::mem::take(&mut pending).into()))
                            .await
                            .is_ok()
                        {
                            pass_through(response, tx).await;
                        }
                        return;
                    }
                }
                Ok(None) => {
                    if !pending.is_empty() {
                        let _ = tx.send(Ok(pending.into())).await;
                    }
                    return;
                }
                Err(err) => {
                    let _ = tx.send(Err(std::io::Error::other(err))).await;
                    return;
                }
            }
        }
    }

    /// `body` re-serialized without the turned-off tools, or `None` when it is
    /// not this request's `tools/list` result.
    fn rewrite(&self, body: &[u8]) -> Option<Vec<u8>> {
        let mut message: Value = serde_json::from_slice(body).ok()?;
        if message.get("id") != Some(&self.id) {
            return None;
        }
        let tools = message
            .get_mut("result")?
            .get_mut("tools")?
            .as_array_mut()?;
        tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_none_or(|name| !self.disabled.contains(name))
        });
        serde_json::to_vec(&message).ok()
    }

    /// One SSE event, blank-line end included, rewritten with its `data:`
    /// lines joined and filtered into one `data:` line. Other lines, such as
    /// `id:` and `event:`, are kept in order. `None` when the event is not this
    /// request's `tools/list` result.
    fn rewrite_event(&self, event: &[u8]) -> Option<Vec<u8>> {
        let text = std::str::from_utf8(event).ok()?;
        let mut data = String::new();
        let mut other = String::new();
        let mut has_data = false;
        for line in text.split("\r\n").flat_map(|l| l.split(['\n', '\r'])) {
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                if has_data {
                    data.push('\n');
                }
                has_data = true;
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            } else {
                other.push_str(line);
                other.push('\n');
            }
        }
        let filtered = self.rewrite(data.as_bytes())?;
        let mut out = other.into_bytes();
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&filtered);
        out.extend_from_slice(b"\n\n");
        Some(out)
    }
}

/// Sends the rest of `response` unchanged.
async fn pass_through(mut response: reqwest::Response, tx: Chunks) {
    loop {
        let next = match response.chunk().await {
            Ok(Some(chunk)) => Ok(chunk),
            Ok(None) => return,
            Err(err) => Err(std::io::Error::other(err)),
        };
        let failed = next.is_err();
        if tx.send(next).await.is_err() || failed {
            return;
        }
    }
}

/// The index just past the first SSE event's blank-line end in `buffer`. SSE
/// lines end in CRLF, LF or CR, so an event ends at the first of `\r\n\r\n`,
/// `\n\n` or `\r\r`.
fn event_end(buffer: &[u8]) -> Option<usize> {
    [&b"\r\n\r\n"[..], b"\n\n", b"\r\r"]
        .iter()
        .filter_map(|end| {
            buffer
                .windows(end.len())
                .position(|window| window == *end)
                .map(|at| at + end.len())
        })
        .min()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|hop| name.as_str().eq_ignore_ascii_case(hop))
}

/// A fresh 256-bit token as 64 lowercase hex characters.
fn random_token() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 0x0f) as usize] as char);
    }
    token
}
