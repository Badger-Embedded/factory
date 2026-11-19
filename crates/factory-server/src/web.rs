use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use factory_core::Event;
use maud::Markup;
use serde::Deserialize;
use tokio::sync::Notify;

use crate::pages;
use crate::projects::Projects;
use crate::settings::Settings;
use crate::store::{Store, StoreError};

/// The header `tailscale serve` sets to the logged-in user's login name.
pub const USER_HEADER: &str = "Tailscale-User-Login";

const LOCAL_USER: &str = "local";
const LOG_LINES: usize = 300;
/// GitHub allows 5000 API calls an hour, and each look at a pull request
/// takes two.
const MIN_POLL_SECONDS: u64 = 10;

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    projects: Arc<Projects>,
    wake: Arc<Notify>,
}

/// The dashboard. Every change it makes to a job notifies `wake`, so the
/// job loop looks at the job right away.
pub fn app(store: Arc<Store>, projects: Arc<Projects>, wake: Arc<Notify>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/style.css", get(stylesheet))
        .route("/jobs", post(create_job))
        .route("/jobs/{id}", get(show_job))
        .route("/jobs/{id}/answers", post(answer))
        .route("/jobs/{id}/approve", post(approve))
        .route("/jobs/{id}/changes", post(request_changes))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/settings", get(show_settings).post(save_settings))
        .with_state(AppState {
            store,
            projects,
            wake,
        })
}

fn user(headers: &HeaderMap) -> String {
    headers
        .get(USER_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|login| !login.is_empty())
        .unwrap_or(LOCAL_USER)
        .to_owned()
}

enum AppError {
    BadRequest(String),
    Store(StoreError),
}

impl From<StoreError> for AppError {
    fn from(err: StoreError) -> Self {
        AppError::Store(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            AppError::Store(err @ StoreError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, err.to_string())
            }
            AppError::Store(err @ StoreError::Rejected(_)) => {
                (StatusCode::CONFLICT, err.to_string())
            }
            AppError::Store(err) => {
                tracing::error!("{err}");
                (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
            }
        };
        (status, pages::error(status, &message)).into_response()
    }
}

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        pages::STYLESHEET,
    )
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Result<Markup, AppError> {
    let jobs = state.store.list()?;
    let settings = state.store.settings()?;
    Ok(pages::index(
        &user(&headers),
        &state.projects,
        &settings,
        &jobs,
    ))
}

#[derive(Deserialize)]
struct NewJob {
    project: String,
    request: String,
}

async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NewJob>,
) -> Result<Redirect, AppError> {
    if state.projects.get(&form.project).is_none() {
        return Err(AppError::BadRequest(format!(
            "unknown project {:?}",
            form.project
        )));
    }
    let request = form.request.trim();
    if request.is_empty() {
        return Err(AppError::BadRequest("the request is empty".into()));
    }
    let id = state
        .store
        .create(&form.project, &user(&headers), request)?;
    Ok(job_redirect(id))
}

async fn show_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Markup, AppError> {
    let row = state.store.get(id)?;
    let logs = state.store.logs(id, LOG_LINES)?;
    let settings = state.store.settings()?;
    Ok(pages::job(&user(&headers), &row, &settings, &logs))
}

async fn answer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    let answers = (0..)
        .map_while(|i| form.get(&format!("answer_{i}")))
        .map(|answer| answer.trim().to_owned())
        .collect();
    apply(&state, id, Event::Answer(answers))
}

async fn approve(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Redirect, AppError> {
    apply(&state, id, Event::Approve)
}

#[derive(Deserialize)]
struct Feedback {
    feedback: String,
}

async fn request_changes(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<Feedback>,
) -> Result<Redirect, AppError> {
    let feedback = form.feedback.trim();
    if feedback.is_empty() {
        return Err(AppError::BadRequest("the feedback is empty".into()));
    }
    apply(
        &state,
        id,
        Event::RequestChanges {
            feedback: feedback.to_owned(),
        },
    )
}

async fn show_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    let settings = state.store.settings()?;
    Ok(pages::settings(&user(&headers), &settings, None))
}

#[derive(Deserialize)]
struct SettingsForm {
    max_workers: usize,
    poll_seconds: u64,
    worker_image: String,
    worker_memory: String,
    worker_cpus: String,
    /// A checkbox: present when ticked.
    auto_merge: Option<String>,
}

async fn save_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> Result<Markup, AppError> {
    let text = |value: String, name: &str| {
        let value = value.trim().to_owned();
        if value.is_empty() {
            Err(AppError::BadRequest(format!("{name} is empty")))
        } else {
            Ok(value)
        }
    };
    if form.poll_seconds < MIN_POLL_SECONDS {
        return Err(AppError::BadRequest(format!(
            "the poll interval must be at least {MIN_POLL_SECONDS} seconds"
        )));
    }
    let settings = Settings {
        max_workers: form.max_workers,
        poll_seconds: form.poll_seconds,
        worker_image: text(form.worker_image, "the worker image")?,
        worker_memory: text(form.worker_memory, "the worker memory")?,
        worker_cpus: text(form.worker_cpus, "the worker CPUs")?,
        auto_merge: form.auto_merge.is_some(),
    };
    state.store.save_settings(&settings)?;
    state.wake.notify_one();
    Ok(pages::settings(&user(&headers), &settings, Some("Saved.")))
}

async fn cancel(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Redirect, AppError> {
    apply(&state, id, Event::Cancel)
}

fn apply(state: &AppState, id: i64, event: Event) -> Result<Redirect, AppError> {
    state.store.apply(id, event)?;
    state.wake.notify_one();
    Ok(job_redirect(id))
}

fn job_redirect(id: i64) -> Redirect {
    Redirect::to(&format!("/jobs/{id}"))
}
