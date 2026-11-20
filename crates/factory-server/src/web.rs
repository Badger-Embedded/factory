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
use maud::{Markup, html};
use serde::Deserialize;
use tokio::sync::Notify;

use crate::github::{RepoChecker, RepoReport};
use crate::pages;
use crate::projects::{Project, Projects, StageModels};
use crate::settings::Settings;
use crate::store::{JobRow, ProjectTotals, Store, StoreError};
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
    github: Arc<dyn RepoChecker>,
}

/// The dashboard. Every change it makes to a job notifies `wake`, so the
/// job loop looks at the job right away. Uploaded images go under
/// `<data_dir>/jobs/<id>/images/`. The index page reads `plan_usage` from the
/// cache the refresh task fills, so it makes no live call to the usage source.
/// The onboarding wizard runs its repository checks through `github`.
pub fn app(
    store: Arc<Store>,
    projects: Arc<Projects>,
    wake: Arc<Notify>,
    data_dir: PathBuf,
    plan_usage: Arc<PlanUsageCache>,
    github: Arc<dyn RepoChecker>,
) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/fragments/plan-usage", get(plan_usage_fragment))
        .route("/fragments/stats", get(stats_fragment))
        .route("/fragments/lanes", get(lanes_fragment))
        .route("/fragments/board", get(board_fragment))
        .route("/style.css", get(stylesheet))
        .route("/htmx.min.js", get(htmx))
        .route("/onboard", get(show_onboard).post(submit_onboard))
        .route(
            "/jobs",
            post(create_job).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/jobs/{id}", get(show_job))
        .route("/jobs/{id}/fragment", get(job_fragment))
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
            github,
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

async fn htmx() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        pages::HTMX,
    )
}

#[derive(Deserialize)]
struct IndexQuery {
    project: Option<String>,
}

/// The store data the board and its fragments render from. Loading it in one
/// place keeps a fragment and the full page on the same data.
struct BoardData {
    names: Vec<String>,
    selected: Option<String>,
    jobs: Vec<JobRow>,
    costs: HashMap<i64, f64>,
    totals: ProjectTotals,
    auto_merge: bool,
}

impl BoardData {
    /// The selected project's jobs, in the store's order.
    fn rows(&self) -> Vec<&JobRow> {
        match self.selected.as_deref() {
            Some(project) => self
                .jobs
                .iter()
                .filter(|row| row.project == project)
                .collect(),
            None => Vec::new(),
        }
    }
}

fn load_board(state: &AppState, project: Option<String>) -> Result<BoardData, AppError> {
    let names = state.projects.names()?;
    let selected = match project {
        Some(name) if !names.iter().any(|known| known == &name) => {
            return Err(AppError::BadRequest(format!("unknown project {name:?}")));
        }
        Some(name) => Some(name),
        None => names.first().cloned(),
    };
    let jobs = state.store.list()?;
    let costs = state.store.costs()?;
    let (totals, auto_merge) = match &selected {
        Some(name) => {
            let default = state.projects.lookup(name)?.is_some_and(|p| p.auto_merge);
            (
                state.store.project_totals(name)?,
                state.store.project_auto_merge(name, default)?,
            )
        }
        None => (ProjectTotals::default(), false),
    };
    Ok(BoardData {
        names,
        selected,
        jobs,
        costs,
        totals,
        auto_merge,
    })
}

async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, AppError> {
    let data = load_board(&state, query.project)?;
    let plan_usage = state.plan_usage.visible(now_secs());
    Ok(pages::index(
        &user(&headers),
        &data.names,
        data.selected.as_deref(),
        &data.jobs,
        &data.costs,
        data.totals,
        plan_usage.as_ref(),
        data.auto_merge,
    ))
}

/// Serves the plan usage tile on its own, from the same cache `index` reads,
/// so a poll makes no live call to the usage endpoint.
async fn plan_usage_fragment(State(state): State<AppState>) -> Markup {
    let plan_usage = state.plan_usage.visible(now_secs());
    pages::plan_usage(plan_usage.as_ref())
}

async fn stats_fragment(
    State(state): State<AppState>,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, AppError> {
    let data = load_board(&state, query.project)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {});
    };
    Ok(pages::stats(data.totals, project))
}

async fn lanes_fragment(
    State(state): State<AppState>,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, AppError> {
    let data = load_board(&state, query.project)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {});
    };
    Ok(pages::lane_counts(&data.rows(), data.auto_merge, project))
}

async fn board_fragment(
    State(state): State<AppState>,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, AppError> {
    let data = load_board(&state, query.project)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {});
    };
    Ok(pages::board(
        &data.rows(),
        &data.costs,
        data.auto_merge,
        project,
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
    if state.projects.lookup(&project)?.is_none() {
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

/// The store data the job page and its fragment render from, loaded in one
/// place so the fragment a poll swaps in cannot disagree with the page.
struct JobData {
    row: JobRow,
    project: Option<Project>,
    settings: Settings,
    logs: Vec<crate::store::LogEntry>,
    cost_usd: f64,
    models: StageModels,
    auto_merge: bool,
}

fn load_job(state: &AppState, id: i64) -> Result<JobData, AppError> {
    let row = state.store.get(id)?;
    let project = state.projects.lookup(&row.project)?;
    let settings = state.store.settings()?;
    let logs = state.store.logs(id, LOG_LINES)?;
    let cost_usd = state.store.cost_usd(id)?;
    let defaults = project
        .as_ref()
        .map(|project| project.models.clone())
        .unwrap_or_default();
    let models = state.store.project_models(&row.project, &defaults)?;
    let default_auto_merge = project.as_ref().is_some_and(|p| p.auto_merge);
    let auto_merge = state
        .store
        .project_auto_merge(&row.project, default_auto_merge)?;
    Ok(JobData {
        row,
        project,
        settings,
        logs,
        cost_usd,
        models,
        auto_merge,
    })
}

async fn show_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Markup, AppError> {
    let data = load_job(&state, id)?;
    let images = state.store.images(id)?;
    Ok(pages::job(
        &user(&headers),
        &data.row,
        data.project.as_ref(),
        &data.settings,
        &data.logs,
        &images,
        data.cost_usd,
        &data.models,
        data.auto_merge,
    ))
}

/// Serves the job page's live regions on their own, from the same data the
/// page reads, so a poll makes no write and no GitHub or usage call. htmx
/// swaps each region into the open page out of band.
async fn job_fragment(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Markup, AppError> {
    let data = load_job(&state, id)?;
    Ok(pages::job_fragment(
        &data.row,
        data.project.as_ref(),
        &data.settings,
        &data.logs,
        data.cost_usd,
        &data.models,
        data.auto_merge,
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
    board_redirect(&state, form.project, redirect)
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
    board_redirect(&state, form.project, redirect)
}

/// Redirects to the project's board when the form names a known project, as
/// the board's own approve and retry buttons do. Otherwise it keeps the job
/// page redirect.
fn board_redirect(
    state: &AppState,
    project: Option<String>,
    fallback: Redirect,
) -> Result<Redirect, AppError> {
    let known = match project {
        Some(name) => state.projects.lookup(&name)?.is_some().then_some(name),
        None => None,
    };
    match known {
        Some(name) => Ok(Redirect::to(&format!("/?project={}", query_escape(&name)))),
        None => Ok(fallback),
    }
}

pub(crate) fn query_escape(value: &str) -> String {
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
        .list()?
        .into_iter()
        .map(|(name, project)| {
            let models = state.store.project_models(&name, &project.models)?;
            let auto_merge = state.store.project_auto_merge(&name, project.auto_merge)?;
            let llm_review = state.store.project_llm_review(&name, project.llm_review)?;
            Ok((name, models, auto_merge, llm_review))
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
    review: String,
    /// Checkboxes: present when ticked.
    auto_merge: Option<String>,
    llm_review: Option<String>,
}

async fn save_project_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<ModelsForm>,
) -> Result<Markup, AppError> {
    if state.projects.lookup(&name)?.is_none() {
        return Err(AppError::UnknownProject(name));
    }
    let model = |value: String| Some(value.trim().to_owned()).filter(|value| !value.is_empty());
    let models = StageModels {
        clarify: model(form.clarify),
        spec: model(form.spec),
        plan: model(form.plan),
        implement: model(form.implement),
        review: model(form.review),
    };
    state.store.save_project_models(&name, &models)?;
    state.store.save_project_settings(
        &name,
        form.auto_merge.is_some(),
        form.llm_review.is_some(),
    )?;
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

/// The base branch an empty field defaults to.
const DEFAULT_BASE: &str = "main";

#[derive(Deserialize)]
struct OnboardForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    base: String,
    #[serde(default)]
    checks: String,
    // Step 2, optional. Empty means the stage runs on the default model.
    #[serde(default)]
    clarify: String,
    #[serde(default)]
    spec: String,
    #[serde(default)]
    plan: String,
    #[serde(default)]
    implement: String,
    #[serde(default)]
    review: String,
    /// Checkboxes: present when ticked.
    auto_merge: Option<String>,
    llm_review: Option<String>,
    /// The button that was pressed: `check`, `create` or `skip`.
    #[serde(default)]
    action: String,
}

async fn show_onboard(headers: HeaderMap) -> Markup {
    let view = pages::OnboardView {
        name: "",
        repo: "",
        base: DEFAULT_BASE,
        checks: "",
        models: &StageModels::default(),
        auto_merge: false,
        llm_review: false,
    };
    pages::onboard(&user(&headers), &view, None)
}

/// Runs the checks, and on `create` or `skip` with no error, onboards the
/// project and redirects to its board. The checks run again here, not only in
/// the earlier `check` step, so a project is never created while a required
/// check fails and a rejection writes no row.
async fn submit_onboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<OnboardForm>,
) -> Result<Response, AppError> {
    let name = form.name.trim();
    let repo = form.repo.trim();
    let base_field = form.base.trim();
    let base = if base_field.is_empty() {
        DEFAULT_BASE
    } else {
        base_field
    };
    let checks = form.checks.trim();
    let effective_name = onboard_name(name, repo);
    let model = |value: &str| Some(value.trim().to_owned()).filter(|value| !value.is_empty());
    let models = StageModels {
        clarify: model(&form.clarify),
        spec: model(&form.spec),
        plan: model(&form.plan),
        implement: model(&form.implement),
        review: model(&form.review),
    };
    let auto_merge = form.auto_merge.is_some();
    let llm_review = form.llm_review.is_some();

    let mut report = onboard_report(&state, effective_name, repo, base).await?;

    if matches!(form.action.as_str(), "create" | "skip") && report.errors.is_empty() {
        let checks = (!checks.is_empty()).then_some(checks);
        match state
            .store
            .create_project(effective_name, repo, base, checks)
        {
            Ok(()) => {
                if form.action == "create" {
                    if models != StageModels::default() {
                        state.store.save_project_models(effective_name, &models)?;
                    }
                    if auto_merge || llm_review {
                        state.store.save_project_settings(
                            effective_name,
                            auto_merge,
                            llm_review,
                        )?;
                    }
                }
                state.wake.notify_one();
                return Ok(
                    Redirect::to(&format!("/?project={}", query_escape(effective_name)))
                        .into_response(),
                );
            }
            // The pre-check caught a duplicate already, but a project onboarded
            // between the check and here would reach this. Show it like any
            // other blocking error, so no row is written twice.
            Err(err @ (StoreError::DuplicateName(_) | StoreError::DuplicateRepo(_))) => {
                report.errors.push(err.to_string());
            }
            Err(err) => return Err(err.into()),
        }
    }

    let view = pages::OnboardView {
        name,
        repo,
        base: base_field,
        checks,
        models: &models,
        auto_merge,
        llm_review,
    };
    Ok(pages::onboard(&user(&headers), &view, Some(&report)).into_response())
}

/// The name a project is created under: the typed name, or the repository name
/// when the name field is empty.
fn onboard_name<'a>(name: &'a str, repo: &'a str) -> &'a str {
    if name.is_empty() {
        repo.rsplit('/').next().unwrap_or(repo)
    } else {
        name
    }
}

/// Whether `name` is a valid project key: non-empty, and only letters, digits,
/// dash, underscore and dot, so it is safe in a URL path and a query.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The repository checks plus the name and repo uniqueness rules. A GitHub call
/// that could not run becomes a blocking error, so the operator sees it and can
/// retry rather than the page returning a 500.
async fn onboard_report(
    state: &AppState,
    name: &str,
    repo: &str,
    base: &str,
) -> Result<RepoReport, AppError> {
    let mut report = match state.github.check(repo, base).await {
        Ok(report) => report,
        Err(err) => RepoReport {
            errors: vec![format!("The repository checks could not run: {err}")],
            warnings: Vec::new(),
        },
    };
    if !valid_name(name) {
        report.errors.insert(
            0,
            format!(
                "The project name {name:?} is empty or has a character other than a letter, digit, dash, underscore or dot."
            ),
        );
    } else if state.store.project(name)?.is_some() {
        report
            .errors
            .insert(0, format!("A project named {name:?} already exists."));
    }
    if state.projects.list()?.iter().any(|(_, p)| p.repo == repo) {
        report
            .errors
            .push(format!("The repository {repo} is already onboarded."));
    }
    Ok(report)
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
