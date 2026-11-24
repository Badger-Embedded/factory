//! The registry of Forgejo servers: validation of a name and a base URL, and
//! the probe that reads a server's version when an admin adds it.

use std::future::Future;
use std::pin::Pin;

use factory_runner::{ForgejoVersion, forgejo_client, forgejo_reported_version};

/// The longest instance name.
const NAME_LIMIT: usize = 32;

/// A registered Forgejo server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoInstance {
    /// Keys the instance in projects, tokens and URLs. It never changes.
    pub name: String,
    /// `https://host[:port][/sub-path]` with no trailing slash.
    pub base_url: String,
    /// The version text the server reported when it was added, such as
    /// `17.0.2+gitea-1.22.0`.
    pub version: String,
    pub added_at: String,
}

impl ForgejoInstance {
    /// Checks an instance name and base URL and returns them in stored form.
    ///
    /// The name is 1 to 32 characters of `a-z`, `0-9` and `-`, and is not
    /// `github` or `salsa`. The URL parses, uses `https`, has a host and has
    /// no user info, query or fragment. A trailing slash is dropped.
    ///
    /// # Errors
    ///
    /// Returns a message that says what is wrong.
    pub fn validate(name: &str, url: &str) -> Result<(String, String), String> {
        if name.is_empty()
            || name.len() > NAME_LIMIT
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(format!(
                "The name must be 1 to {NAME_LIMIT} characters of a-z, 0-9 and -."
            ));
        }
        if matches!(name, "github" | "salsa") {
            return Err(format!(
                "{name:?} is a built-in forge, so pick another name."
            ));
        }
        Ok((name.to_owned(), normalize_url(url)?))
    }

    /// What this server's version cannot do, one sentence each.
    #[must_use]
    pub fn warnings(&self) -> Vec<&'static str> {
        ForgejoVersion::parse(&self.version).map_or_else(|_| Vec::new(), version_warnings)
    }
}

/// What a server of `version` cannot do.
#[must_use]
pub fn version_warnings(version: ForgejoVersion) -> Vec<&'static str> {
    let mut warnings = Vec::new();
    if !version.reruns_jobs() {
        warnings.push("This version cannot rerun failed CI. A failed check stays failed until someone reruns it on the server.");
    }
    if !version.reads_job_logs() {
        warnings.push("This version cannot serve failed job logs, so the agent gets a failed check's name and link without a log.");
    }
    warnings
}

fn normalize_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.chars().any(|c| c.is_ascii_control()) {
        return Err("The URL must not hold control characters.".into());
    }
    let rest = match url.get(..8) {
        Some(prefix) if prefix.eq_ignore_ascii_case("https://") => &url[8..],
        _ => return Err("The URL must use https://.".into()),
    };
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("The URL needs a host.".into());
    }
    if authority.contains('@') {
        return Err("The URL must not hold a user name or password.".into());
    }
    let parsed = url::Url::parse(url).map_err(|err| format!("The URL is invalid: {err}."))?;
    if parsed.scheme() != "https" {
        return Err("The URL must use https.".into());
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("The URL needs a host.".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("The URL must not hold a user name or password.".into());
    }
    if parsed.query().is_some() {
        return Err("The URL must not hold a query.".into());
    }
    if parsed.fragment().is_some() {
        return Err("The URL must not hold a fragment.".into());
    }
    let origin = parsed.origin().ascii_serialization();
    Ok(format!("{origin}{}", parsed.path().trim_end_matches('/')))
}

/// The answer of a probe: the version text the server reported, and parsed.
pub type ProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(String, ForgejoVersion), String>> + Send + 'a>>;

/// Reads the version of a Forgejo server. A trait so the dashboard's tests can
/// answer without a network.
pub trait ForgejoProbe: Send + Sync {
    /// Reads the version of the server at `base_url`, as the text it reported
    /// and as parsed.
    ///
    /// # Errors
    ///
    /// Returns a message when the server does not answer or does not answer
    /// as Forgejo.
    fn version<'a>(&'a self, base_url: &'a str) -> ProbeFuture<'a>;
}

/// The production [`ForgejoProbe`]. It trusts `roots` next to the built-in
/// roots.
pub struct HttpForgejoProbe {
    client: Result<reqwest::Client, String>,
}

impl HttpForgejoProbe {
    #[must_use]
    pub fn new(roots: &[reqwest::Certificate]) -> Self {
        HttpForgejoProbe {
            client: forgejo_client(roots),
        }
    }
}

impl ForgejoProbe for HttpForgejoProbe {
    fn version<'a>(&'a self, base_url: &'a str) -> ProbeFuture<'a> {
        Box::pin(async move {
            match &self.client {
                Ok(client) => forgejo_reported_version(base_url, client).await,
                Err(err) => Err(err.clone()),
            }
        })
    }
}
