//! The MCP server registry: the remote servers an operator adds on the
//! settings page, and the validation their fields must pass.
//!
//! A server is a name, an `https` URL and a list of header names each project
//! may fill with a value of its own. The name is the key in a run's MCP config
//! and part of the proxy path, so it is a tight slug. The header names are the
//! only headers a project may set, so the factory refuses the ones it sets
//! itself or that the streamable-HTTP protocol owns. The [`Store`](crate::Store)
//! holds the rows; this module owns the type and the rules.

use std::fmt;

use axum::http::Uri;

use crate::store::StoreError;

/// Header names the factory sets itself or the streamable-HTTP protocol owns.
/// A project must not declare one, because the proxy would overwrite it or the
/// protocol would break. Compared case-insensitively, as HTTP header names are.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// A remote MCP server in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// 1 to 32 characters of `a-z`, `0-9` and `-`, unique across the registry.
    pub name: String,
    /// The server's streamable-HTTP endpoint. Always `https`.
    pub url: String,
    /// The header names a project may give a value for. May be empty. Each is a
    /// valid HTTP token and not reserved; no two differ only in case.
    pub headers: Vec<String>,
}

/// The stages a server may run in for a project: the same five as the project
/// models. A project picks a subset for each server it enables. `false`
/// everywhere, which [`Default`] gives, means no stage is picked.
// One bool per stage mirrors the five model stages; a bitset would read worse.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpStages {
    pub clarify: bool,
    pub spec: bool,
    pub plan: bool,
    pub implement: bool,
    pub review: bool,
}

impl McpStages {
    /// Whether any stage is picked. An enabled server with none is rejected.
    #[must_use]
    pub fn any(self) -> bool {
        self.clarify || self.spec || self.plan || self.implement || self.review
    }
}

/// A project's settings for one MCP server: whether it is on and the stages it
/// runs in. [`Default`] is off with no stage, the state a project reads for a
/// server it has no stored row for, so a new server and a newly onboarded
/// project both start off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpProjectSettings {
    pub enabled: bool,
    pub stages: McpStages,
}

/// A tool an MCP server offered in its `tools/list` reply, as the factory keeps
/// it for the settings page: the name a `tools/call` uses and the server's
/// description, when it gave one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
}

/// A stored tool of a project's server, as the settings page lists it.
// Each flag is a separate fact about the row the page shows on its own.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolState {
    pub name: String,
    pub description: Option<String>,
    /// Whether the last complete `tools/list` reply had the tool.
    pub offered: bool,
    /// Whether the project turned the tool off.
    pub disabled: bool,
    /// Whether the latest refresh saw the tool for the first time and an
    /// earlier refresh had already listed the server's tools.
    pub new: bool,
}

/// A failure validating or storing an MCP server. The validation variants map
/// to a 400 in the dashboard; [`McpError::NotFound`] maps to a 404.
#[derive(Debug)]
pub enum McpError {
    /// The name is empty, too long, or has a character outside `a-z0-9-`.
    BadName(String),
    /// A server already exists under this name.
    DuplicateName(String),
    /// No server is registered under this name.
    NotFound(String),
    /// The URL does not parse, is not `https`, or has no host.
    BadUrl(String),
    /// A header name is not a valid HTTP token.
    BadHeader(String),
    /// A header name is one the factory sets itself or the protocol owns.
    ReservedHeader(String),
    /// A project enabled a server without picking a stage for it.
    NoStage(String),
    Store(StoreError),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::BadName(name) => write!(
                f,
                "{name:?} is not a valid server name; use 1 to 32 characters of a-z, 0-9 and -"
            ),
            McpError::DuplicateName(name) => write!(f, "a server named {name:?} already exists"),
            McpError::NotFound(name) => write!(f, "no MCP server named {name:?}"),
            McpError::BadUrl(reason) => write!(f, "{reason}"),
            McpError::BadHeader(header) => {
                write!(f, "{header:?} is not a valid HTTP header name")
            }
            McpError::ReservedHeader(header) => write!(
                f,
                "the header {header:?} is reserved by the factory or the protocol and cannot be declared"
            ),
            McpError::NoStage(server) => write!(
                f,
                "the server {server:?} is enabled but no stage is checked; pick at least one or turn it off"
            ),
            McpError::Store(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for McpError {}

impl From<StoreError> for McpError {
    fn from(err: StoreError) -> Self {
        McpError::Store(err)
    }
}

impl From<rusqlite::Error> for McpError {
    fn from(err: rusqlite::Error) -> Self {
        McpError::Store(StoreError::Db(err))
    }
}

impl From<serde_json::Error> for McpError {
    fn from(err: serde_json::Error) -> Self {
        McpError::Store(StoreError::Json(err))
    }
}

/// Checks a server name against `[a-z0-9-]{1,32}`.
///
/// The "Add a server" form carries the same rule as an HTML `pattern` on
/// `#mcp-new-name` in pages.rs, so the browser blocks a bad name before
/// submit. Change both together. A browser compiles that `pattern` with the
/// `v` flag, where `-` and `|` are syntax inside a `[...]` class and must be
/// escaped; an invalid pattern is silently ignored, so the form writes the
/// trailing `-` as `\-`. This check stays the authority: a name from curl or a
/// stale page that skips the browser still gets rejected here.
///
/// # Errors
///
/// Returns [`McpError::BadName`] when the name is empty, longer than 32
/// characters, or carries a character outside the set.
pub(crate) fn validate_name(name: &str) -> Result<(), McpError> {
    let ok = (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if ok {
        Ok(())
    } else {
        Err(McpError::BadName(name.to_owned()))
    }
}

/// Checks that a URL parses, uses `https` and has a host.
///
/// # Errors
///
/// Returns [`McpError::BadUrl`] otherwise.
pub(crate) fn validate_url(url: &str) -> Result<(), McpError> {
    let uri: Uri = url
        .parse()
        .map_err(|_| McpError::BadUrl(format!("{url:?} is not a valid URL")))?;
    if uri.scheme_str() != Some("https") {
        return Err(McpError::BadUrl(format!("the URL {url:?} must use https")));
    }
    if uri.host().is_none() {
        return Err(McpError::BadUrl(format!("the URL {url:?} has no host")));
    }
    Ok(())
}

/// Validates each header name and drops a case-insensitive duplicate, keeping
/// the first spelling. The result is what the registry stores.
///
/// # Errors
///
/// Returns [`McpError::BadHeader`] for a name that is not an HTTP token and
/// [`McpError::ReservedHeader`] for a reserved one.
pub(crate) fn normalize_headers(headers: &[String]) -> Result<Vec<String>, McpError> {
    let mut kept: Vec<String> = Vec::new();
    for header in headers {
        validate_header(header)?;
        if !kept.iter().any(|seen| seen.eq_ignore_ascii_case(header)) {
            kept.push(header.clone());
        }
    }
    Ok(kept)
}

fn validate_header(header: &str) -> Result<(), McpError> {
    if header.is_empty() || !header.bytes().all(is_token_char) {
        return Err(McpError::BadHeader(header.to_owned()));
    }
    if RESERVED_HEADERS
        .iter()
        .any(|reserved| header.eq_ignore_ascii_case(reserved))
    {
        return Err(McpError::ReservedHeader(header.to_owned()));
    }
    Ok(())
}

/// A `tchar` from RFC 9110: the characters a header field name may hold.
fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// Whether two URLs share an origin: scheme, host and port. The scheme is
/// always `https` here, so this compares the host case-insensitively and the
/// port, with 443 standing in for an absent one. A URL that fails to parse is
/// treated as a different origin, so an edit then clears the stored values, the
/// safe side.
pub(crate) fn same_origin(old: &str, new: &str) -> bool {
    match (old.parse::<Uri>(), new.parse::<Uri>()) {
        (Ok(old), Ok(new)) => origin(&old) == origin(&new),
        _ => false,
    }
}

fn origin(uri: &Uri) -> (Option<String>, Option<String>, u16) {
    (
        uri.scheme_str().map(str::to_ascii_lowercase),
        uri.host().map(str::to_ascii_lowercase),
        uri.port_u16().unwrap_or(443),
    )
}
