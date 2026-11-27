//! The `OpenRouter` agent's API: the list of models it serves, read from
//! `GET /api/v1/models`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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

pub type OpenRouterKeyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ModelsError>> + Send + 'a>>;

pub type OpenRouterGenerationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<f64, GenerationError>> + Send + 'a>>;

/// Why a generation's cost could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationError {
    /// `OpenRouter` does not know the generation yet, or its answer holds no
    /// cost yet. It can show up a moment after the run ends.
    NotReady,
    /// The request failed.
    Failed(ModelsError),
}

impl GenerationError {
    /// Whether asking again later can help. A 401 or 403 means `OpenRouter`
    /// refuses the key, which waiting does not change.
    #[must_use]
    pub fn retryable(&self) -> bool {
        !matches!(
            self,
            GenerationError::Failed(ModelsError::Status(401 | 403))
        )
    }
}

impl std::fmt::Display for GenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerationError::NotReady => f.write_str("OpenRouter has no cost for it yet"),
            GenerationError::Failed(error) => error.fmt(f),
        }
    }
}

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

    /// Asks `OpenRouter` about `key` with one `GET /api/v1/key`, so a run
    /// finds a rejected key before its worker starts.
    ///
    /// # Errors
    ///
    /// Returns [`ModelsError::Status`] with the HTTP status when `OpenRouter`
    /// does not accept the key, and [`ModelsError::Connect`] or
    /// [`ModelsError::Timeout`] when it cannot be reached. The text never holds
    /// the key.
    fn check_key<'a>(&'a self, key: &'a str) -> OpenRouterKeyFuture<'a>;

    /// Reads what `OpenRouter` charged for generation `id` in USD, with one
    /// `GET /api/v1/generation?id=<id>` using `key`.
    ///
    /// # Errors
    ///
    /// Returns [`GenerationError::NotReady`] for a 404 or an answer with no
    /// `data.total_cost`, which can hold right after a run, and
    /// [`GenerationError::Failed`] for any other failure. The text never holds
    /// the key.
    fn generation<'a>(&'a self, id: &'a str, key: &'a str) -> OpenRouterGenerationFuture<'a>;
}

/// Reads what a run was charged from the generations its output showed.
pub struct OpenRouterCosts {
    api: Arc<dyn OpenRouterApi>,
    delays: Vec<Duration>,
    deadline: Duration,
}

impl OpenRouterCosts {
    /// Asks again after 2, 4, 8, 16 and 30 seconds, and gives up after 90
    /// seconds in all, requests included.
    #[must_use]
    pub fn new(api: Arc<dyn OpenRouterApi>) -> Self {
        Self::with_limits(
            api,
            [2, 4, 8, 16, 30].map(Duration::from_secs).to_vec(),
            Duration::from_secs(90),
        )
    }

    /// As [`Self::new`], with the waits between the rounds of asking and the
    /// time the whole lookup may take given.
    #[must_use]
    pub fn with_limits(
        api: Arc<dyn OpenRouterApi>,
        delays: Vec<Duration>,
        deadline: Duration,
    ) -> Self {
        OpenRouterCosts {
            api,
            delays,
            deadline,
        }
    }

    /// The sum of the costs of the generations `ids`, read with `key`. Each
    /// round asks only for the ids still unread. `None` when an id stays
    /// unread after the last round or the deadline, `OpenRouter` refuses the
    /// key, or `ids` is empty. A lookup that runs out of time returns no
    /// partial sum.
    pub async fn total(&self, key: &str, ids: &[String]) -> Option<f64> {
        if ids.is_empty() {
            return None;
        }
        tokio::time::timeout(self.deadline, self.read(key, ids))
            .await
            .ok()
            .flatten()
    }

    async fn read(&self, key: &str, ids: &[String]) -> Option<f64> {
        let mut costs: Vec<Option<f64>> = vec![None; ids.len()];
        for round in 0..=self.delays.len() {
            for (id, slot) in ids.iter().zip(&mut costs) {
                if slot.is_some() {
                    continue;
                }
                match self.api.generation(id, key).await {
                    Ok(cost) => *slot = Some(cost),
                    Err(error) if !error.retryable() => return None,
                    Err(_) => {}
                }
            }
            if costs.iter().all(Option::is_some) {
                break;
            }
            if let Some(delay) = self.delays.get(round) {
                tokio::time::sleep(*delay).await;
            }
        }
        costs.into_iter().sum()
    }
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

    fn check_key<'a>(&'a self, key: &'a str) -> OpenRouterKeyFuture<'a> {
        Box::pin(async move {
            let client = self.client.as_ref().ok_or(ModelsError::Connect)?;
            let response = client
                .get(format!("{}/v1/key", self.base_url))
                .bearer_auth(key)
                .send()
                .await
                .map_err(|err| transport(&err))?;
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else {
                Err(ModelsError::Status(status.as_u16()))
            }
        })
    }

    fn generation<'a>(&'a self, id: &'a str, key: &'a str) -> OpenRouterGenerationFuture<'a> {
        Box::pin(async move {
            let client = self
                .client
                .as_ref()
                .ok_or(GenerationError::Failed(ModelsError::Connect))?;
            let response = client
                .get(format!("{}/v1/generation", self.base_url))
                .query(&[("id", id)])
                .bearer_auth(key)
                .send()
                .await
                .map_err(|err| GenerationError::Failed(transport(&err)))?;
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(GenerationError::NotReady);
            }
            if !status.is_success() {
                return Err(GenerationError::Failed(ModelsError::Status(
                    status.as_u16(),
                )));
            }
            let body = response
                .bytes()
                .await
                .map_err(|err| GenerationError::Failed(transport(&err)))?;
            parse_generation_cost(&body)
        })
    }
}

/// The `data.total_cost` of a generation answer.
///
/// # Errors
///
/// Returns [`GenerationError::NotReady`] when the body holds no non-negative
/// cost.
pub fn parse_generation_cost(body: &[u8]) -> Result<f64, GenerationError> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("data")?.get("total_cost")?.as_f64())
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .ok_or(GenerationError::NotReady)
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
            None => false,
            Some(serde_json::Value::Array(parameters)) => {
                let mut tools = false;
                for parameter in parameters {
                    let name = parameter.as_str().ok_or(ModelsError::NotAModelList)?;
                    tools |= name == "tools";
                }
                tools
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
