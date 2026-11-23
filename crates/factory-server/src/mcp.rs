//! The MCP server registry: the servers an operator adds on the settings page,
//! and the validation their fields must pass.
//!
//! A server is a name and a transport. A remote server is an `https` URL and a
//! list of header names each project may fill with a value of its own. A stdio
//! server is a command and base arguments the agent starts inside the worker.
//! The name is the key in a run's MCP config
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

/// An MCP server in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// 1 to 32 characters of `a-z`, `0-9` and `-`, unique across the registry.
    pub name: String,
    pub transport: McpTransport,
}

/// How a run reaches an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// A streamable-HTTP server the factory proxy forwards to.
    Remote {
        /// The server's endpoint. Always `https`.
        url: String,
        /// The header names a project may give a value for. May be empty. Each
        /// is a valid HTTP token and not reserved; no two differ only in case.
        headers: Vec<String>,
    },
    /// A process the agent starts inside the worker and talks to over stdin
    /// and stdout. It bypasses the proxy, so it has no stored values.
    Stdio {
        /// The program to run, found on the worker's `PATH`. Trimmed.
        command: String,
        /// The base arguments, in order, each one argv element. Kept as typed:
        /// not trimmed, never split or expanded by a shell.
        args: Vec<String>,
    },
}

impl McpTransport {
    /// The kind's name as the registry stores it and the pages show it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            McpTransport::Remote { .. } => "remote",
            McpTransport::Stdio { .. } => "stdio",
        }
    }
}

impl McpServer {
    #[must_use]
    pub fn remote(name: impl Into<String>, url: impl Into<String>, headers: Vec<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Remote {
                url: url.into(),
                headers,
            },
        }
    }

    #[must_use]
    pub fn stdio(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args,
            },
        }
    }

    /// The URL and header names when the server is remote.
    #[must_use]
    pub fn as_remote(&self) -> Option<(&str, &[String])> {
        match &self.transport {
            McpTransport::Remote { url, headers } => Some((url, headers)),
            McpTransport::Stdio { .. } => None,
        }
    }

    /// The command and base arguments when the server is stdio.
    #[must_use]
    pub fn as_stdio(&self) -> Option<(&str, &[String])> {
        match &self.transport {
            McpTransport::Stdio { command, args } => Some((command, args)),
            McpTransport::Remote { .. } => None,
        }
    }
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
    /// A stdio server's command is empty, too long, or has a NUL, a newline or
    /// `${`.
    BadCommand(String),
    /// An argument list has too many entries, or an entry that is too long or
    /// has a NUL or `${`.
    BadArgs(String),
    /// A posted kind is neither `remote` nor `stdio`.
    BadKind(String),
    /// An edit would change a server's kind, which is fixed when it is added.
    KindChange(String),
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
            McpError::BadUrl(reason)
            | McpError::BadCommand(reason)
            | McpError::BadArgs(reason)
            | McpError::BadKind(reason) => {
                write!(f, "{reason}")
            }
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
            McpError::KindChange(name) => write!(
                f,
                "the server {name:?} cannot change kind; delete it and add it again"
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

/// The longest stdio command, in bytes.
const MAX_COMMAND_BYTES: usize = 256;
/// The most arguments one list may hold.
pub(crate) const MAX_ARGS: usize = 64;
/// The longest single argument, in bytes.
pub(crate) const MAX_ARG_BYTES: usize = 4096;

/// Checks a stdio server's command and base arguments and returns them as the
/// registry stores them: the command trimmed and the empty arguments dropped.
///
/// `${` is refused because Copilot expands `${NAME}` in the command and
/// arguments of its MCP config and the other agents do not, so refusing it
/// keeps one entry meaning the same on all three.
///
/// # Errors
///
/// Returns [`McpError::BadCommand`] or [`McpError::BadArgs`] naming the problem.
pub(crate) fn validate_stdio(
    command: &str,
    args: &[String],
) -> Result<(String, Vec<String>), McpError> {
    if command.contains(['\n', '\r']) {
        return Err(McpError::BadCommand(
            "the command has a line break".to_owned(),
        ));
    }
    let command = command.trim();
    let bad = |reason: &str| Err(McpError::BadCommand(format!("the command {reason}")));
    if command.is_empty() {
        return bad("is empty");
    }
    if command.len() > MAX_COMMAND_BYTES {
        return bad(&format!("is longer than {MAX_COMMAND_BYTES} bytes"));
    }
    if command.contains('\0') {
        return bad("has a NUL");
    }
    if command.contains("${") {
        return bad("contains \"${\", which Copilot would expand");
    }
    Ok((command.to_owned(), validate_args(args)?))
}

/// Drops the empty arguments and checks the rest: at most 64, each at most
/// 4096 bytes, with no NUL or `${`. A line break is allowed. Order is kept and no
/// argument is trimmed, so spaces, quotes and a lone `$` arrive unchanged.
///
/// # Errors
///
/// Returns [`McpError::BadArgs`] naming the problem.
pub(crate) fn validate_args(args: &[String]) -> Result<Vec<String>, McpError> {
    let kept: Vec<String> = args.iter().filter(|arg| !arg.is_empty()).cloned().collect();
    if kept.len() > MAX_ARGS {
        return Err(McpError::BadArgs(format!(
            "there are {} arguments; the most allowed is {MAX_ARGS}",
            kept.len()
        )));
    }
    for arg in &kept {
        if arg.len() > MAX_ARG_BYTES {
            return Err(McpError::BadArgs(format!(
                "an argument is longer than {MAX_ARG_BYTES} bytes"
            )));
        }
        if arg.contains('\0') {
            return Err(McpError::BadArgs("an argument contains a NUL".to_owned()));
        }
        if arg.contains("${") {
            return Err(McpError::BadArgs(format!(
                "the argument {arg:?} contains \"${{\", which Copilot would expand"
            )));
        }
    }
    Ok(kept)
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
