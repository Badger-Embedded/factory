//! The Local agent's server: the URL it is stored under and the list of models
//! it serves, read from the OpenAI-compatible `/v1/models` endpoint.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use url::Url;

const MODELS_TIMEOUT: Duration = Duration::from_secs(10);

/// The form a server URL is stored in: `http` or `https`, a host and an
/// optional port, with no trailing slash and no trailing `/v1`.
///
/// # Errors
///
/// Returns the problem as a message for the page when the URL is not one the
/// factory accepts.
pub fn normalize_server_url(input: &str) -> Result<String, String> {
    let parsed = Url::parse(input.trim())
        .map_err(|_| "The server URL is not a URL. Write it as http://host:port.".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("The server URL must start with http:// or https://.".into());
    }
    if parsed.host_str().is_none() {
        return Err("The server URL must name a host.".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("The server URL must not hold a user name or password.".into());
    }
    if parsed.query().is_some() {
        return Err("The server URL must not hold a query.".into());
    }
    if parsed.fragment().is_some() {
        return Err("The server URL must not hold a fragment.".into());
    }
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let path = path.strip_suffix("/v1").unwrap_or(path);
    Ok(format!("{origin}{}", path.trim_end_matches('/')))
}

/// Why the model list could not be read. The text never holds the API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsError {
    /// The connection failed.
    Connect,
    /// The server did not answer in time.
    Timeout,
    /// The server answered with this HTTP status.
    Status(u16),
    /// The answer is not an `OpenAI` model list.
    NotAModelList,
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelsError::Connect => f.write_str("cannot connect to the server"),
            ModelsError::Timeout => f.write_str("the server did not answer within 10 seconds"),
            ModelsError::Status(status) => write!(f, "the server answered HTTP {status}"),
            ModelsError::NotAModelList => f.write_str("the answer is not a model list"),
        }
    }
}

/// The answer of a model list request.
pub type ModelsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<String>, ModelsError>> + Send + 'a>>;

/// Reads the models a local server serves. A trait so tests can answer without
/// a network.
pub trait LocalServerProbe: Send + Sync {
    /// Lists the model ids at `<base_url>/v1/models`, sending `key` as a bearer
    /// token when there is one.
    ///
    /// # Errors
    ///
    /// Returns why the list could not be read.
    fn models<'a>(&'a self, base_url: &'a str, key: Option<&'a str>) -> ModelsFuture<'a>;
}

/// The production [`LocalServerProbe`]. It follows no redirect, so the key never
/// goes to another host.
pub struct HttpLocalServerProbe {
    client: Option<reqwest::Client>,
}

impl HttpLocalServerProbe {
    #[must_use]
    pub fn new() -> Self {
        HttpLocalServerProbe {
            client: reqwest::Client::builder()
                .timeout(MODELS_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok(),
        }
    }
}

impl Default for HttpLocalServerProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalServerProbe for HttpLocalServerProbe {
    fn models<'a>(&'a self, base_url: &'a str, key: Option<&'a str>) -> ModelsFuture<'a> {
        Box::pin(async move {
            let client = self.client.as_ref().ok_or(ModelsError::Connect)?;
            let mut request = client.get(format!("{base_url}/v1/models"));
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.map_err(|err| {
                if err.is_timeout() {
                    ModelsError::Timeout
                } else {
                    ModelsError::Connect
                }
            })?;
            let status = response.status();
            if !status.is_success() {
                return Err(ModelsError::Status(status.as_u16()));
            }
            let body = response.bytes().await.map_err(|err| {
                if err.is_timeout() {
                    ModelsError::Timeout
                } else {
                    ModelsError::Connect
                }
            })?;
            parse_models(&body)
        })
    }
}

fn parse_models(body: &[u8]) -> Result<Vec<String>, ModelsError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| ModelsError::NotAModelList)?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or(ModelsError::NotAModelList)?;
    data.iter()
        .map(|entry| {
            entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or(ModelsError::NotAModelList)
        })
        .collect()
}
