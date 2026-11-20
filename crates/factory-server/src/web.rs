use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::multipart::{Field, Multipart, MultipartError};
use axum::extract::{DefaultBodyLimit, Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use factory_core::{Event, Gates};
use maud::Markup;
use serde::Deserialize;
use tokio::sync::Notify;

use crate::pages;
use crate::projects::{Projects, StageModels};
use crate::settings::Settings;
use crate::store::{ProjectTotals, Store, StoreError};
use crate::usage::PlanUsageCache;

/// The header `tailscale serve` sets to the logged-in user's login name.
pub const USER_HEADER: &str = "Tailscale-User-Login";

const LOCAL_USER: &str = "local";
const LOG_LINES: usize = 300;
/// GitHub allows 5000 API calls an hour, and each look at a pull request
/// takes two.
const MIN_POLL_SECONDS: u64 = 10;

/// Images one request may carry.
pub const MAX_IMAGES: usize = 5;
/// Bytes in one image.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Bytes of the request text and the other form fields together.
const MAX_FORM_BYTES: usize = 256 * 1024;
const MAX_UPLOAD_BYTES: usize = MAX_IMAGES * MAX_IMAGE_BYTES + MAX_FORM_BYTES;

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    projects: Arc<Projects>,
    wake: Arc<Notify>,
    data_dir: PathBuf,
    plan_usage: Arc<PlanUsageCache>,
}

/// The dashboard. Every change it makes to a job notifies `wake`, so the
/// job loop looks at the job right away. Uploaded images go under
/// `<data_dir>/jobs/<id>/images/`. The index page reads `plan_usage` from the
/// cache the refresh task fills, so it makes no live call to the usage source.
pub fn app(
    store: Arc<Store>,
    projects: Arc<Projects>,
    wake: Arc<Notify>,
    data_dir: PathBuf,
    plan_usage: Arc<PlanUsageCache>,
) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/style.css", get(stylesheet))
        .route(
            "/jobs",
            post(create_job).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/jobs/{id}", get(show_job))
        .route("/jobs/{id}/images/{name}", get(show_image))
        .route("/jobs/{id}/answers", post(answer))
        .route("/jobs/{id}/approve", post(approve))
        .route("/jobs/{id}/retry", post(retry))
        .route("/jobs/{id}/changes", post(request_changes))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/settings", get(show_settings).post(save_settings))
        .route("/settings/projects/{name}", post(save_project_models))
        .with_state(AppState {
            store,
            projects,
            wake,
            data_dir,
            plan_usage,
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
    Io(String),
    UnknownProject(String),
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
            AppError::UnknownProject(name) => (StatusCode::NOT_FOUND, format!("no project {name}")),
            AppError::Io(message) => {
                tracing::error!("{message}");
                (StatusCode::INTERNAL_SERVER_ERROR, message)
            }
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

#[derive(Deserialize)]
struct IndexQuery {
    project: Option<String>,
}

/// The project's auto-merge default from its project file, false when the
/// project is unknown. A saved row in `project_settings` replaces it.
fn toml_auto_merge(state: &AppState, project: &str) -> bool {
    state.projects.get(project).is_some_and(|p| p.auto_merge)
}

async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, AppError> {
    let selected = match query.project {
        Some(name) if state.projects.get(&name).is_none() => {
            return Err(AppError::BadRequest(format!("unknown project {name:?}")));
        }
        Some(name) => Some(name),
        None => state.projects.names().next().map(str::to_owned),
    };
    let jobs = state.store.list()?;
    let costs = state.store.costs()?;
    let (totals, auto_merge) = match &selected {
        Some(name) => (
            state.store.project_totals(name)?,
            state
                .store
                .project_auto_merge(name, toml_auto_merge(&state, name))?,
        ),
        None => (ProjectTotals::default(), false),
    };
    let plan_usage = state.plan_usage.visible(now_secs());
    Ok(pages::index(
        &user(&headers),
        &state.projects,
        selected.as_deref(),
        &jobs,
        &costs,
        totals,
        plan_usage.as_ref(),
        auto_merge,
    ))
}

/// The current Unix time in seconds, or 0 before the epoch.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

struct Upload {
    extension: &'static str,
    bytes: Vec<u8>,
}

/// The file extension for the image format `bytes` starts with.
fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn multipart_error(err: &MultipartError) -> AppError {
    AppError::BadRequest(format!(
        "the upload is malformed or over {MAX_UPLOAD_BYTES} bytes: {}",
        err.body_text()
    ))
}

async fn read_text(field: Field<'_>) -> Result<String, AppError> {
    field.text().await.map_err(|err| multipart_error(&err))
}

/// Reads one image part. Browsers send an empty part with no file name when
/// no file is chosen, and that part is not an image.
async fn read_image(mut field: Field<'_>, uploads: &mut Vec<Upload>) -> Result<(), AppError> {
    let named = field.file_name().is_some_and(|name| !name.is_empty());
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|err| multipart_error(&err))? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(AppError::BadRequest(format!(
                "an image is over the limit of {MAX_IMAGE_BYTES} bytes"
            )));
        }
    }
    if bytes.is_empty() && !named {
        return Ok(());
    }
    let Some(extension) = image_extension(&bytes) else {
        return Err(AppError::BadRequest(
            "an upload is not a PNG, JPEG, GIF or WebP image".into(),
        ));
    };
    if uploads.len() == MAX_IMAGES {
        return Err(AppError::BadRequest(format!(
            "a request takes at most {MAX_IMAGES} images"
        )));
    }
    uploads.push(Upload { extension, bytes });
    Ok(())
}

async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    let mut project = None;
    let mut request = None;
    let mut uploads = Vec::new();
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|err| multipart_error(&err))?
    {
        match field.name() {
            Some("project") => project = Some(read_text(field).await?),
            Some("request") => request = Some(read_text(field).await?),
            Some("images") => read_image(field, &mut uploads).await?,
            _ => {}
        }
    }
    let project = project.unwrap_or_default();
    if state.projects.get(&project).is_none() {
        return Err(AppError::BadRequest(format!("unknown project {project:?}")));
    }
    let request = request.unwrap_or_default();
    let request = request.trim();
    if request.is_empty() {
        return Err(AppError::BadRequest("the request is empty".into()));
    }
    let id = state.store.create(&project, &user(&headers), request)?;
    if !uploads.is_empty()
        && let Err(err) = save_images(&state, id, &uploads)
    {
        state.store.apply(id, Event::Cancel)?;
        return Err(err);
    }
    Ok(job_redirect(id))
}

/// Writes the images under generated names, so an upload's own file name
/// never reaches the file system, and records the names with the job.
fn save_images(state: &AppState, id: i64, uploads: &[Upload]) -> Result<(), AppError> {
    let dir = state
        .data_dir
        .join("jobs")
        .join(id.to_string())
        .join("images");
    let io = |err: std::io::Error| AppError::Io(format!("cannot save the images: {err}"));
    std::fs::create_dir_all(&dir).map_err(io)?;
    let mut names = Vec::new();
    for (index, upload) in uploads.iter().enumerate() {
        let name = format!("image-{}.{}", index + 1, upload.extension);
        std::fs::write(dir.join(&name), &upload.bytes).map_err(io)?;
        names.push(name);
    }
    state.store.add_images(id, &names)?;
    Ok(())
}

async fn show_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Markup, AppError> {
    let row = state.store.get(id)?;
    let logs = state.store.logs(id, LOG_LINES)?;
    let settings = state.store.settings()?;
    let project = state.projects.get(&row.project);
    let images = state.store.images(id)?;
    let cost_usd = state.store.cost_usd(id)?;
    let defaults = project
        .map(|project| project.models.clone())
        .unwrap_or_default();
    let models = state.store.project_models(&row.project, &defaults)?;
    let auto_merge = state
        .store
        .project_auto_merge(&row.project, toml_auto_merge(&state, &row.project))?;
    Ok(pages::job(
        &user(&headers),
        &row,
        project,
        &settings,
        &logs,
        &images,
        cost_usd,
        &models,
        auto_merge,
    ))
}

/// Serves an image the store lists for the job. The name must match a
/// stored one, so a request cannot reach any other file.
async fn show_image(
    State(state): State<AppState>,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    let not_found = || AppError::from(StoreError::NotFound(id));
    let content_type = match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Err(not_found()),
    };
    if !state.store.images(id)?.contains(&name) {
        return Err(not_found());
    }
    let path = state
        .data_dir
        .join("jobs")
        .join(id.to_string())
        .join("images")
        .join(&name);
    let bytes = tokio::fs::read(path).await.map_err(|_| not_found())?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
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

#[derive(Deserialize)]
struct ApproveForm {
    /// Set by the job board, so the redirect returns to that project's board.
    project: Option<String>,
}

async fn approve(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<ApproveForm>,
) -> Result<Redirect, AppError> {
    let redirect = apply(&state, id, Event::Approve)?;
    match form
        .project
        .filter(|name| state.projects.get(name).is_some())
    {
        Some(name) => Ok(Redirect::to(&format!("/?project={}", query_escape(&name)))),
        None => Ok(redirect),
    }
}

async fn retry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<ApproveForm>,
) -> Result<Redirect, AppError> {
    let redirect = apply(&state, id, Event::Retry)?;
    state
        .store
        .append_factory_log(id, "Retried the job from the dashboard.")?;
    match form
        .project
        .filter(|name| state.projects.get(name).is_some())
    {
        Some(name) => Ok(Redirect::to(&format!("/?project={}", query_escape(&name)))),
        None => Ok(redirect),
    }
}

fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
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
    settings_page(&state, &headers, None)
}

fn settings_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    let settings = state.store.settings()?;
    let projects = state
        .projects
        .names()
        .map(|name| {
            let defaults = state
                .projects
                .get(name)
                .map(|project| project.models.clone())
                .unwrap_or_default();
            Ok((
                name.to_owned(),
                state.store.project_models(name, &defaults)?,
                state
                    .store
                    .project_auto_merge(name, toml_auto_merge(state, name))?,
            ))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(pages::settings(
        &user(headers),
        &settings,
        &projects,
        notice,
    ))
}

#[derive(Deserialize)]
struct ModelsForm {
    clarify: String,
    spec: String,
    plan: String,
    implement: String,
    /// A checkbox: present when ticked.
    auto_merge: Option<String>,
}

async fn save_project_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<ModelsForm>,
) -> Result<Markup, AppError> {
    if state.projects.get(&name).is_none() {
        return Err(AppError::UnknownProject(name));
    }
    let model = |value: String| Some(value.trim().to_owned()).filter(|value| !value.is_empty());
    let models = StageModels {
        clarify: model(form.clarify),
        spec: model(form.spec),
        plan: model(form.plan),
        implement: model(form.implement),
    };
    state.store.save_project_models(&name, &models)?;
    state
        .store
        .save_project_auto_merge(&name, form.auto_merge.is_some())?;
    state.wake.notify_one();
    settings_page(&state, &headers, Some("Saved."))
}

#[derive(Deserialize)]
struct SettingsForm {
    max_workers: usize,
    poll_seconds: u64,
    worker_image: String,
    worker_memory: String,
    worker_cpus: String,
    /// Checkboxes: present when ticked.
    gate_spec: Option<String>,
    gate_plan: Option<String>,
    gate_publish: Option<String>,
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
        gates: Gates {
            spec: form.gate_spec.is_some(),
            plan: form.gate_plan.is_some(),
            publish: form.gate_publish.is_some(),
        },
    };
    state.store.save_settings(&settings)?;
    state.wake.notify_one();
    settings_page(&state, &headers, Some("Saved."))
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
