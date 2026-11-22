//! The per-project MCP connection check behind the "Refresh connection"
//! button.
//!
//! A project's server block on the settings page can test the server with the
//! project's saved header values, the same values the proxy would send. The
//! check sends a JSON-RPC `initialize` request and reports whether a valid MCP
//! reply came back. After a valid reply it lists the server's tools with
//! `tools/list`, which the dashboard stores for the project. It follows no
//! redirect, so a header value never reaches another host.
//!
//! The HTTP part sits behind the [`McpChecker`] trait, held by the dashboard
//! the way it holds [`RepoChecker`](crate::github::RepoChecker). Production uses
//! [`HttpMcpChecker`]; a test points a fake at the handler and exercises the
//! real client against a local `http://` server.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::mcp::McpTool;

/// The connect timeout, the same as the proxy's. The host must accept a
/// connection within this window.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The whole check's timeout, the tool listing included. A server that accepts
/// the connection but does not answer `initialize` within this window is
/// reported as a timeout; one that answers it but not the listing gets
/// [`ToolList::Failed`].
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

/// The most of a response body the check reads before it gives up looking for
/// an MCP reply. A valid `initialize` response is small; a server that streams
/// megabytes is not answering the handshake.
const MAX_BODY: usize = 1024 * 1024;

/// The MCP protocol version the check advertises in its `initialize` request.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The `clientInfo.name` the check sends, so a server's logs show who called.
const CLIENT_NAME: &str = "factory";

/// The `clientInfo.version` the check sends. The MCP schema requires it, and a
/// server that validates the request, such as Composio's, rejects an
/// `initialize` without it.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The most `tools/list` pages the check follows. A server whose cursor never
/// ends gets [`ToolList::Failed`] rather than an endless loop.
const MAX_TOOL_PAGES: usize = 100;

/// The most characters of a server's JSON-RPC error message the result shows.
const MAX_ERROR_MESSAGE: usize = 300;

/// What the check found. It carries no header value, so a result rendered into
/// the page, written to a log or put in an error leaks no credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// A 2xx reply that parsed as a valid `initialize` response. The server's
    /// own name and version are optional, as a server may omit them.
    Ok {
        server_name: Option<String>,
        server_version: Option<String>,
        protocol_version: String,
        tools: ToolList,
    },
    /// The connection could not be made: DNS, a refused connection, or TLS.
    Unreachable,
    /// The server accepted the connection but did not answer in time.
    Timeout,
    /// The server answered 401 or 403.
    Refused { status: u16 },
    /// The server answered another non-2xx status. `location` carries a 3xx
    /// `Location` header when there is one.
    HttpStatus {
        status: u16,
        location: Option<String>,
    },
    /// A 2xx reply carrying a JSON-RPC error instead of a result. `message` is
    /// the server's own text, cut to [`MAX_ERROR_MESSAGE`] characters, with
    /// every header value the check sent replaced by `[redacted]`.
    RpcError { code: i64, message: String },
    /// A 2xx reply that parsed as neither an `initialize` response nor a
    /// JSON-RPC error.
    NotMcp,
}

/// What listing the server's tools after a valid `initialize` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolList {
    /// Every page of `tools/list`, in the server's order. Empty for a server
    /// that declares no `tools` capability.
    Listed(Vec<McpTool>),
    /// The listing failed or was cut short, with a reason that names no header
    /// value. The stored list stays as it was.
    Failed(String),
}

/// Runs the MCP connection check behind a trait object, so the dashboard holds
/// one without naming the concrete client. A test passes a fake that returns a
/// chosen outcome and records the headers it was given.
pub trait McpChecker: Send + Sync {
    /// Sends the `initialize` request to `url` with `headers` added, and reports
    /// what came back. `headers` are the project's resolved header values; the
    /// check adds them to its own `Content-Type` and `Accept`.
    fn check<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'a>>;
}

/// The production [`McpChecker`]: a reqwest client that follows no redirect and
/// uses the proxy's connect timeout. The whole check is bounded by
/// [`TOTAL_TIMEOUT`].
pub struct HttpMcpChecker {
    client: reqwest::Client,
    total_timeout: Duration,
}

impl HttpMcpChecker {
    /// Builds the checker with the production timeouts.
    ///
    /// # Panics
    ///
    /// As [`HttpMcpChecker::with_timeout`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(TOTAL_TIMEOUT)
    }

    /// Builds the checker with a chosen total timeout, so a test need not wait
    /// the full production window for the timeout case.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be built, which needs no network and
    /// happens only on a broken TLS backend, as the proxy's client does.
    #[must_use]
    pub fn with_timeout(total_timeout: Duration) -> Self {
        Self::with_roots_and_timeout(Vec::new(), total_timeout)
    }

    /// Builds the checker with the production timeouts and `roots` trusted next
    /// to the built-in roots, for a server whose certificate is not from a
    /// public CA.
    ///
    /// # Panics
    ///
    /// As [`HttpMcpChecker::with_timeout`].
    #[must_use]
    pub fn with_roots(roots: Vec<reqwest::Certificate>) -> Self {
        Self::with_roots_and_timeout(roots, TOTAL_TIMEOUT)
    }

    /// As [`HttpMcpChecker::with_roots`], with a chosen total timeout.
    ///
    /// # Panics
    ///
    /// As [`HttpMcpChecker::with_timeout`].
    #[must_use]
    pub fn with_roots_and_timeout(
        roots: Vec<reqwest::Certificate>,
        total_timeout: Duration,
    ) -> Self {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT);
        for root in roots {
            builder = builder.add_root_certificate(root);
        }
        let client = builder.build().expect("build the MCP check HTTP client");
        HttpMcpChecker {
            client,
            total_timeout,
        }
    }
}

impl Default for HttpMcpChecker {
    fn default() -> Self {
        HttpMcpChecker::new()
    }
}

impl McpChecker for HttpMcpChecker {
    fn check<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'a>> {
        Box::pin(async move {
            let deadline = Instant::now() + self.total_timeout;
            self.run(url, &headers, deadline).await
        })
    }
}

impl HttpMcpChecker {
    /// Sends `initialize`, classifies the reply, and lists the tools after a
    /// valid one, all before `deadline`.
    async fn run(
        &self,
        url: &str,
        headers: &[(String, String)],
        deadline: Instant,
    ) -> CheckOutcome {
        let Ok(outcome) = tokio::time::timeout_at(deadline, self.initialize(url, headers)).await
        else {
            return CheckOutcome::Timeout;
        };
        let (outcome, session) = outcome;
        let CheckOutcome::Ok {
            server_name,
            server_version,
            protocol_version,
            ..
        } = outcome
        else {
            return outcome;
        };
        let session = session.unwrap_or_default();
        let tools = if session.has_tools {
            let listing = self.list_tools(url, headers, &protocol_version, session.id.as_deref());
            match tokio::time::timeout_at(deadline, listing).await {
                Ok(tools) => tools,
                Err(_elapsed) => {
                    ToolList::Failed("the server did not list its tools in time".to_owned())
                }
            }
        } else {
            ToolList::Listed(Vec::new())
        };
        if let Some(id) = &session.id {
            let _ = tokio::time::timeout_at(deadline, self.end_session(url, headers, id)).await;
        }
        CheckOutcome::Ok {
            server_name,
            server_version,
            protocol_version,
            tools,
        }
    }

    /// Sends `initialize` and classifies the reply. On a valid reply it also
    /// returns the session the server opened.
    async fn initialize(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> (CheckOutcome, Option<Session>) {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
            },
        });
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .body(serde_json::to_vec(&body).unwrap_or_default());
        for (name, value) in headers {
            request = request.header(name, value);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(err) if err.is_timeout() => return (CheckOutcome::Timeout, None),
            // A refused connection, an unknown host or a TLS failure. The error
            // is not shown, so no header value can leak through it.
            Err(_) => return (CheckOutcome::Unreachable, None),
        };

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return (
                CheckOutcome::Refused {
                    status: status.as_u16(),
                },
                None,
            );
        }
        if !status.is_success() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            return (
                CheckOutcome::HttpStatus {
                    status: status.as_u16(),
                    location,
                },
                None,
            );
        }

        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match read_reply(response, parse_rpc).await {
            Some(Reply::Error { code, message }) => (
                CheckOutcome::RpcError {
                    code,
                    message: redact(&message, headers),
                },
                None,
            ),
            Some(Reply::Init(init)) => (
                CheckOutcome::Ok {
                    server_name: init.server_name,
                    server_version: init.server_version,
                    protocol_version: init.protocol_version,
                    tools: ToolList::Listed(Vec::new()),
                },
                Some(Session {
                    id: session_id,
                    has_tools: init.has_tools,
                }),
            ),
            None => (CheckOutcome::NotMcp, None),
        }
    }

    /// Sends `notifications/initialized`, then pages through `tools/list` by
    /// its cursor. Each request carries the negotiated protocol version and the
    /// session id, as the streamable-HTTP transport requires after the
    /// handshake.
    async fn list_tools(
        &self,
        url: &str,
        headers: &[(String, String)],
        protocol_version: &str,
        session: Option<&str>,
    ) -> ToolList {
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        if let Err(reason) = self
            .post(url, headers, protocol_version, session, &initialized)
            .await
        {
            return ToolList::Failed(reason);
        }
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for page in 0..MAX_TOOL_PAGES {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let request = json!({
                "jsonrpc": "2.0",
                "id": page + 2,
                "method": "tools/list",
                "params": params,
            });
            let response = match self
                .post(url, headers, protocol_version, session, &request)
                .await
            {
                Ok(response) => response,
                Err(reason) => return ToolList::Failed(reason),
            };
            match read_reply(response, parse_tools_page).await {
                Some(Ok(page)) => {
                    tools.extend(page.tools);
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => return ToolList::Listed(tools),
                    }
                }
                Some(Err((code, message))) => {
                    return ToolList::Failed(format!(
                        "tools/list returned error {code}: {}",
                        redact(&message, headers)
                    ));
                }
                None => {
                    return ToolList::Failed("the tools/list reply was not valid".to_owned());
                }
            }
        }
        ToolList::Failed(format!(
            "the server returned more than {MAX_TOOL_PAGES} pages of tools"
        ))
    }

    /// POSTs one JSON-RPC message in an open session and returns a 2xx
    /// response, or a reason that names no header value.
    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        protocol_version: &str,
        session: Option<&str>,
        message: &Value,
    ) -> Result<reqwest::Response, String> {
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header("mcp-protocol-version", protocol_version)
            .body(serde_json::to_vec(message).unwrap_or_default());
        if let Some(id) = session {
            request = request.header("mcp-session-id", id);
        }
        for (name, value) in headers {
            request = request.header(name, value);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => Ok(response),
            Ok(response) => Err(format!(
                "the server answered HTTP {} while listing tools",
                response.status().as_u16()
            )),
            Err(_) => Err("the request to list tools failed".to_owned()),
        }
    }

    /// Sends a best-effort `DELETE` to end the session the server opened, and
    /// ignores the outcome, the way a well-behaved MCP client closes a session.
    async fn end_session(&self, url: &str, headers: &[(String, String)], session_id: &str) {
        let mut request = self.client.delete(url).header("mcp-session-id", session_id);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let _ = request.send().await;
    }
}

/// The session an `initialize` reply opened: the server's `Mcp-Session-Id`,
/// if it sent one, and whether it declared the `tools` capability.
#[derive(Default)]
struct Session {
    id: Option<String>,
    has_tools: bool,
}

/// One page of a `tools/list` result.
struct ToolsPage {
    tools: Vec<McpTool>,
    next_cursor: Option<String>,
}

/// What a JSON-RPC response to `initialize` held.
enum Reply {
    Init(Initialize),
    Error { code: i64, message: String },
}

/// Cuts a server's error message to [`MAX_ERROR_MESSAGE`] characters and
/// replaces each header value the check sent. A server may echo the request
/// back, and the result is shown to anyone who can open the settings page.
fn redact(message: &str, headers: &[(String, String)]) -> String {
    let mut text = message.to_owned();
    for (_, value) in headers {
        if !value.is_empty() {
            text = text.replace(value.as_str(), "[redacted]");
        }
    }
    if text.chars().count() > MAX_ERROR_MESSAGE {
        text = text.chars().take(MAX_ERROR_MESSAGE).collect();
        text.push_str("...");
    }
    text
}

/// A parsed `initialize` result: its protocol version and the server's own
/// name and version, when it gives them.
struct Initialize {
    server_name: Option<String>,
    server_version: Option<String>,
    protocol_version: String,
    has_tools: bool,
}

/// Reads the response body, up to [`MAX_BODY`], and parses a reply from it
/// with `parse`. The body may be plain JSON or an SSE stream. The first
/// `data:` event `parse` accepts counts. Returns `None` when no such reply
/// arrives.
async fn read_reply<T>(mut response: reqwest::Response, parse: fn(&str) -> Option<T>) -> Option<T> {
    let mut buffer = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                buffer.extend_from_slice(&chunk);
                let capped = buffer.len() >= MAX_BODY;
                if capped {
                    buffer.truncate(MAX_BODY);
                }
                if let Ok(text) = std::str::from_utf8(&buffer)
                    && let Some(reply) = extract_reply(text, parse)
                {
                    return Some(reply);
                }
                if capped {
                    return None;
                }
            }
            // The stream ended: a final parse covers a body that arrived whole.
            Ok(None) => {
                return std::str::from_utf8(&buffer)
                    .ok()
                    .and_then(|text| extract_reply(text, parse));
            }
            Err(_) => return None,
        }
    }
}

/// Finds a reply in `body`, read as plain JSON or as SSE. For SSE
/// it joins the `data:` lines of each event and parses each event in turn.
fn extract_reply<T>(body: &str, parse_rpc: fn(&str) -> Option<T>) -> Option<T> {
    if let Some(reply) = parse_rpc(body) {
        return Some(reply);
    }
    let mut data = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() {
            if let Some(init) = parse_rpc(&data) {
                return Some(init);
            }
            data.clear();
        }
    }
    if data.is_empty() {
        None
    } else {
        parse_rpc(&data)
    }
}

/// Parses `text` as a JSON-RPC response. An `error` object is a reply; a
/// `result` must carry a `protocolVersion` string for the handshake to count.
fn parse_rpc(text: &str) -> Option<Reply> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Some(Reply::Error { code, message });
    }
    let result = value.get("result")?;
    let protocol_version = result.get("protocolVersion")?.as_str()?.to_owned();
    let server_info = result.get("serverInfo");
    let server_name = server_info
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let server_version = server_info
        .and_then(|info| info.get("version"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let has_tools = result
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("tools"))
        .is_some_and(|tools| !tools.is_null());
    Some(Reply::Init(Initialize {
        server_name,
        server_version,
        protocol_version,
        has_tools,
    }))
}

/// Parses `text` as a `tools/list` response: a page of tools, or a JSON-RPC
/// error's code and message. A tool with no string `name` is skipped.
fn parse_tools_page(text: &str) -> Option<Result<ToolsPage, (i64, String)>> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Some(Err((code, message)));
    }
    let result = value.get("result")?;
    let tools = result
        .get("tools")?
        .as_array()?
        .iter()
        .filter_map(|tool| {
            Some(McpTool {
                name: tool.get("name")?.as_str()?.to_owned(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect();
    let next_cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(Ok(ToolsPage { tools, next_cursor }))
}
