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

/// Why the model could not be loaded. The text never holds the API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The connection failed.
    Connect,
    /// The server did not answer within the limit.
    Timeout,
    /// The server answered with this HTTP status and, when its body held one,
    /// this error message.
    Status {
        status: u16,
        message: Option<String>,
    },
}

/// The answer of a model load request.
pub type LoadFuture<'a> = Pin<Box<dyn Future<Output = Result<(), LoadError>> + Send + 'a>>;

/// Reads the models a local server serves, and loads one. A trait so tests can answer without
/// a network.
pub trait LocalServerProbe: Send + Sync {
    /// Lists the model ids at `<base_url>/v1/models`, sending `key` as a bearer
    /// token when there is one.
    ///
    /// # Errors
    ///
    /// Returns why the list could not be read.
    fn models<'a>(&'a self, base_url: &'a str, key: Option<&'a str>) -> ModelsFuture<'a>;

    /// Sends one `max_tokens: 1` chat completion for `model` to
    /// `<base_url>/v1/chat/completions`. The server answers once it has loaded
    /// the model, so the answer is the proof that a worker can start. Waits at
    /// most `timeout`.
    ///
    /// # Errors
    ///
    /// Returns why the model did not answer with a success status.
    fn load<'a>(
        &'a self,
        base_url: &'a str,
        model: &'a str,
        key: Option<&'a str>,
        timeout: Duration,
    ) -> LoadFuture<'a>;
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

    fn load<'a>(
        &'a self,
        base_url: &'a str,
        model: &'a str,
        key: Option<&'a str>,
        timeout: Duration,
    ) -> LoadFuture<'a> {
        Box::pin(async move {
            let client = self.client.as_ref().ok_or(LoadError::Connect)?;
            let mut request = client
                .post(format!("{base_url}/v1/chat/completions"))
                .timeout(timeout)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(
                    serde_json::json!({
                        "model": model,
                        "messages": [{"role": "user", "content": "hi"}],
                        "max_tokens": 1,
                    })
                    .to_string(),
                );
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.map_err(|err| {
                if err.is_timeout() {
                    LoadError::Timeout
                } else {
                    LoadError::Connect
                }
            })?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            // The page for a rejected key needs only the status, so a server
            // that stalls the body of a 401 or 403 must not hold the run.
            if matches!(status.as_u16(), 401 | 403) {
                return Err(LoadError::Status {
                    status: status.as_u16(),
                    message: None,
                });
            }
            let body = read_bounded(response).await;
            Err(LoadError::Status {
                status: status.as_u16(),
                message: error_message(&body, key),
            })
        })
    }
}

const ERROR_BODY_LIMIT: usize = 16 * 1024;
const ERROR_BODY_DEADLINE: Duration = Duration::from_secs(2);
const ERROR_MESSAGE_LIMIT: usize = 300;

/// Reads what arrives of the body within the deadline, so a stalled body costs
/// the run no more than the deadline and the status still stands.
async fn read_bounded(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    let _ = tokio::time::timeout(ERROR_BODY_DEADLINE, async {
        while body.len() < ERROR_BODY_LIMIT {
            let Ok(Some(chunk)) = response.chunk().await else {
                break;
            };
            let room = ERROR_BODY_LIMIT - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
    })
    .await;
    body
}

/// The `error.message` of an `OpenAI`-style error body, or its `error` when that
/// is a string. The key is redacted from the whole message before it is cut
/// short, so a key that straddles the cut cannot leave a prefix behind.
fn error_message(body: &[u8], key: Option<&str>) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.as_str())?
        .trim();
    let message = match key {
        Some(key) if !key.is_empty() => message.replace(key, "<redacted>"),
        _ => message.to_owned(),
    };
    (!message.is_empty()).then(|| message.chars().take(ERROR_MESSAGE_LIMIT).collect())
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
