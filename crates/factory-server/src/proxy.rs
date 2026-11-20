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
//! connect timeout or a 5xx answer goes to a [`WarnFn`] callback, which records
//! the server as unavailable for the run.

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

/// The production [`Upstreams`]: the server's URL comes from the registry and
/// each stored header value from the encrypted secret store. A header with no
/// stored value, or one that does not decrypt, is left off, so it is never
/// forwarded.
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
        let mut headers = Vec::new();
        for header in &registered.headers {
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
        Some(Upstream {
            url: registered.url,
            headers,
        })
    }
}

/// A proxy-side reason a server was unavailable for a run: the job, the server,
/// and a short reason with no credential in it. The factory records it as a
/// warning on the job.
pub struct ProxyWarning {
    pub job: i64,
    pub server: String,
    pub reason: String,
}

/// The callback the proxy calls when it cannot reach a server, a connection
/// times out, or a server answers 5xx.
pub type WarnFn = Arc<dyn Fn(ProxyWarning) + Send + Sync>;

#[derive(Clone)]
struct ProxyState {
    tokens: Arc<RunTokens>,
    upstreams: Arc<dyn Upstreams>,
    client: reqwest::Client,
    warn: WarnFn,
}

/// The proxy router. It listens on the factory's MCP port, separate from the
/// dashboard, and serves `/mcp/<server>` with any method the streamable-HTTP
/// transport uses. `tokens` gates each request, `upstreams` resolves the
/// server, and `warn` records a server the proxy could not reach.
///
/// # Panics
///
/// Panics if the HTTP client cannot be built, which needs no network and
/// happens only on a broken TLS backend.
pub fn proxy_app(tokens: Arc<RunTokens>, upstreams: Arc<dyn Upstreams>, warn: WarnFn) -> Router {
    // No redirect is followed, so a 3xx cannot carry the project's headers to
    // another host; it is returned to the worker as-is. A connect timeout
    // bounds the wait to reach a server; there is no total timeout, so a
    // streaming response may stay open as long as the run.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("build the MCP proxy HTTP client");
    let state = ProxyState {
        tokens,
        upstreams,
        client,
        warn,
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
        (state.warn)(ProxyWarning {
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
                (state.warn)(ProxyWarning {
                    job: scope.job,
                    server,
                    reason: format!("the server answered {}", response.status()),
                });
            }
            relay(response)
        }
        Err(err) => {
            let reason = if err.is_timeout() {
                "the connection to the server timed out"
            } else if err.is_connect() {
                "the server could not be reached"
            } else {
                "the request to the server failed"
            };
            (state.warn)(ProxyWarning {
                job: scope.job,
                server,
                reason: reason.to_owned(),
            });
            unreachable_response()
        }
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
