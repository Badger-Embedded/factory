//! The `OpenRouter` agent's API: the list of models it serves, read from
//! `GET /api/v1/models`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::local_server::ModelsError;

/// Where `OpenRouter` answers. Claude Code takes it as is and the other CLIs
/// append `/v1`.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api";

const MODELS_TIMEOUT: Duration = Duration::from_secs(10);

/// One model of the `OpenRouter` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
}

/// The answer of a model list request.
pub type OpenRouterModelsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<OpenRouterModel>, ModelsError>> + Send + 'a>>;

/// Talks to `OpenRouter`. A trait so tests can answer without a network.
pub trait OpenRouterApi: Send + Sync {
    /// Lists the models that support tool calls, sending `key` as a bearer
    /// token when there is one. Every CLI needs tool calls, so a model without
    /// them is left out.
    ///
    /// # Errors
    ///
    /// Returns why the list could not be read. The text never holds the key.
    fn models<'a>(&'a self, key: Option<&'a str>) -> OpenRouterModelsFuture<'a>;
}

/// The production [`OpenRouterApi`]. It follows no redirect, so the key never
/// goes to another host.
pub struct HttpOpenRouterApi {
    base_url: String,
    client: Option<reqwest::Client>,
}

impl HttpOpenRouterApi {
    #[must_use]
    pub fn new() -> Self {
        Self::at(OPENROUTER_BASE_URL)
    }

    /// As [`Self::new`], against another host, for the browser tests.
    #[cfg(feature = "e2e-fakes")]
    #[must_use]
    pub fn with_base_url(base_url: &str) -> Self {
        Self::at(base_url)
    }

    fn at(base_url: &str) -> Self {
        HttpOpenRouterApi {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .timeout(MODELS_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok(),
        }
    }
}

impl Default for HttpOpenRouterApi {
    fn default() -> Self {
        Self::new()
    }
}

fn transport(err: &reqwest::Error) -> ModelsError {
    if err.is_timeout() {
        ModelsError::Timeout
    } else {
        ModelsError::Connect
    }
}

impl OpenRouterApi for HttpOpenRouterApi {
    fn models<'a>(&'a self, key: Option<&'a str>) -> OpenRouterModelsFuture<'a> {
        Box::pin(async move {
            let client = self.client.as_ref().ok_or(ModelsError::Connect)?;
            let mut request = client.get(format!("{}/v1/models", self.base_url));
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.map_err(|err| transport(&err))?;
            let status = response.status();
            if !status.is_success() {
                return Err(ModelsError::Status(status.as_u16()));
            }
            let body = response.bytes().await.map_err(|err| transport(&err))?;
            parse_models(&body)
        })
    }
}

/// The models of an `OpenRouter` list that support tools, in its order. A model
/// with no name is listed under its id.
///
/// # Errors
///
/// Returns [`ModelsError::NotAModelList`] when the body is not a list of
/// models with ids.
pub fn parse_models(body: &[u8]) -> Result<Vec<OpenRouterModel>, ModelsError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| ModelsError::NotAModelList)?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or(ModelsError::NotAModelList)?;
    let mut models = Vec::new();
    for entry in data {
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or(ModelsError::NotAModelList)?;
        let tools = match entry.get("supported_parameters") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Array(parameters)) => {
                parameters.iter().any(|p| p.as_str() == Some("tools"))
            }
            Some(_) => return Err(ModelsError::NotAModelList),
        };
        if !tools {
            continue;
        }
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(id);
        models.push(OpenRouterModel {
            id: id.to_owned(),
            name: name.to_owned(),
        });
    }
    Ok(models)
}
