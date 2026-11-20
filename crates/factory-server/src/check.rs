//! The per-project MCP connection check behind the "Check connection" button.
//!
//! A project's server block on the settings page can test the server with the
//! project's saved header values, the same values the proxy would send. The
//! check sends one JSON-RPC `initialize` request and reports whether a valid
//! MCP reply came back. It follows no redirect, so a header value never reaches
//! another host, and it stores nothing: the result lives only in the fragment
//! htmx swaps in.
//!
//! The HTTP part sits behind the [`McpChecker`] trait, held by the dashboard
//! the way it holds [`RepoChecker`](crate::github::RepoChecker). Production uses
//! [`HttpMcpChecker`]; a test points a fake at the handler and exercises the
//! real client against a local `http://` server.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};

/// The connect timeout, the same as the proxy's. The host must accept a
/// connection within this window.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The whole check's timeout. A server that accepts the connection but does not
/// answer within this window is reported as a timeout.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

/// The most of a response body the check reads before it gives up looking for
/// an MCP reply. A valid `initialize` response is small; a server that streams
/// megabytes is not answering the handshake.
const MAX_BODY: usize = 1024 * 1024;

/// The MCP protocol version the check advertises in its `initialize` request.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The `clientInfo.name` the check sends, so a server's logs show who called.
const CLIENT_NAME: &str = "factory";

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
    /// A 2xx reply that did not parse as an `initialize` response.
    NotMcp,
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
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("build the MCP check HTTP client");
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
            match tokio::time::timeout(self.total_timeout, self.run(url, &headers)).await {
                Ok(outcome) => outcome,
                Err(_elapsed) => CheckOutcome::Timeout,
            }
        })
    }
}

impl HttpMcpChecker {
    /// Sends the request and classifies the reply. The caller wraps this in the
    /// total timeout.
    async fn run(&self, url: &str, headers: &[(String, String)]) -> CheckOutcome {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": CLIENT_NAME },
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
            Err(err) if err.is_timeout() => return CheckOutcome::Timeout,
            // A refused connection, an unknown host or a TLS failure. The error
            // is not shown, so no header value can leak through it.
            Err(_) => return CheckOutcome::Unreachable,
        };

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return CheckOutcome::Refused {
                status: status.as_u16(),
            };
        }
        if !status.is_success() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            return CheckOutcome::HttpStatus {
                status: status.as_u16(),
                location,
            };
        }

        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match read_initialize(response).await {
            Some(init) => {
                if let Some(id) = session_id {
                    self.end_session(url, headers, &id).await;
                }
                CheckOutcome::Ok {
                    server_name: init.server_name,
                    server_version: init.server_version,
                    protocol_version: init.protocol_version,
                }
            }
            None => CheckOutcome::NotMcp,
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

/// A parsed `initialize` result: its protocol version and the server's own
/// name and version, when it gives them.
struct Initialize {
    server_name: Option<String>,
    server_version: Option<String>,
    protocol_version: String,
}

/// Reads the response body, up to [`MAX_BODY`], and parses an `initialize`
/// reply from it. The body may be plain JSON or an SSE stream; the first
/// `data:` event that carries a JSON-RPC response with a `result.protocolVersion`
/// counts. Returns `None` when no such reply arrives.
async fn read_initialize(mut response: reqwest::Response) -> Option<Initialize> {
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
                    && let Some(init) = extract_initialize(text)
                {
                    return Some(init);
                }
                if capped {
                    return None;
                }
            }
            // The stream ended: a final parse covers a body that arrived whole.
            Ok(None) => {
                return std::str::from_utf8(&buffer)
                    .ok()
                    .and_then(extract_initialize);
            }
            Err(_) => return None,
        }
    }
}

/// Finds an `initialize` reply in `body`, read as plain JSON or as SSE. For SSE
/// it joins the `data:` lines of each event and parses each event in turn.
fn extract_initialize(body: &str) -> Option<Initialize> {
    if let Some(init) = parse_rpc(body) {
        return Some(init);
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

/// Parses `text` as a JSON-RPC response and reads its `initialize` result. The
/// result must carry a `protocolVersion` string for the handshake to count.
fn parse_rpc(text: &str) -> Option<Initialize> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
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
    Some(Initialize {
        server_name,
        server_version,
        protocol_version,
    })
}
