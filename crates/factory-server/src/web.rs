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

use crate::check::{CheckOutcome, McpChecker, ToolList};
use crate::github::{RepoChecker, RepoReport};
use crate::mcp::{McpError, McpProjectSettings, McpServer, McpStages};
use crate::pages::{self, HeaderState, ProjectSettings, ServerSettings, Workers};
use crate::projects::{Project, Projects, StageEfforts, StageModels};
use crate::proxy::{StoreUpstreams, Upstream, Upstreams};
use crate::secrets::{SecretError, SecretRef, SecretStore};
use crate::settings::Settings;
use crate::store::{JobRow, ProjectTotals, Store, StoreError};
use crate::usage::PlanUsageCache;
use crate::workers::WorkerSlots;

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
    slots: Arc<WorkerSlots>,
    secrets: Arc<dyn SecretStore>,
    checker: Arc<dyn McpChecker>,
}

/// The dashboard. Every change it makes to a job notifies `wake`, so the
/// job loop looks at the job right away. Uploaded images go under
/// `<data_dir>/jobs/<id>/images/`. The index page reads `plan_usage` from the
/// cache the refresh task fills, so it makes no live call to the usage source.
/// The onboarding wizard runs its repository checks through `github`. The
/// board shows the worker slots the scheduler fills in `slots`. Per-project
/// MCP header values are read and written through `secrets`, which reports
/// itself unavailable when no master key is present. The per-project MCP
/// connection check runs through `checker`, the way the onboarding checks run
/// through `github`.
#[allow(
    clippy::too_many_arguments,
    reason = "the dashboard wires each long-lived dependency the handlers share"
)]
pub fn app(
    store: Arc<Store>,
    projects: Arc<Projects>,
    wake: Arc<Notify>,
    data_dir: PathBuf,
    plan_usage: Arc<PlanUsageCache>,
    github: Arc<dyn RepoChecker>,
    slots: Arc<WorkerSlots>,
    secrets: Arc<dyn SecretStore>,
    checker: Arc<dyn McpChecker>,
) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/fragments/plan-usage", get(plan_usage_fragment))
        .route("/fragments/stats", get(stats_fragment))
        .route("/fragments/workers", get(workers_fragment))
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
        .route("/settings", get(settings_home))
        .route("/settings/workers", get(show_workers).post(save_workers))
        .route("/settings/gates", get(show_gates).post(save_gates))
        .route("/settings/github", get(show_github).post(save_github))
        .route("/settings/claude", get(show_claude))
        .route(
            "/settings/projects/{name}",
            get(show_project_settings).post(save_project_models),
        )
        .route("/settings/claude-token", post(save_claude_token))
        .route("/settings/claude-token/clear", post(clear_claude_token))
        .route("/settings/mcp", get(show_mcp).post(add_mcp_server))
        .route(
            "/settings/mcp/{name}",
            get(show_mcp_server).post(edit_mcp_server),
        )
        .route("/settings/mcp/{name}/delete", post(delete_mcp_server))
        .route(
            "/settings/projects/{project}/mcp/{server}",
            get(show_project_mcp).post(save_project_mcp),
        )
        .route(
            "/settings/projects/{project}/mcp/{server}/check",
            post(check_mcp_server),
        )
        .with_state(AppState {
            store,
            projects,
            wake,
            data_dir,
            plan_usage,
            github,
            slots,
            secrets,
            checker,
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
    NotFound(String),
    Io(String),
    UnknownProject(String),
    Store(StoreError),
}

impl From<StoreError> for AppError {
    fn from(err: StoreError) -> Self {
        AppError::Store(err)
    }
}

impl From<McpError> for AppError {
    fn from(err: McpError) -> Self {
        match err {
            McpError::Store(err) => AppError::Store(err),
            err @ McpError::NotFound(_) => AppError::NotFound(err.to_string()),
            err => AppError::BadRequest(err.to_string()),
        }
    }
}

impl From<SecretError> for AppError {
    fn from(err: SecretError) -> Self {
        match err {
            SecretError::Unavailable => AppError::BadRequest(err.to_string()),
            SecretError::Store(err) => AppError::Store(err),
            // Decrypt, Encrypt and UnknownBackend are internal faults. None of
            // them carries a plaintext value, so the message leaks nothing.
            err => AppError::Io(err.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            AppError::NotFound(message) => (StatusCode::NOT_FOUND, message),
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
            AppError::Store(err @ StoreError::InvalidEffort(_)) => {
                (StatusCode::BAD_REQUEST, err.to_string())
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
    workers: Workers,
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
        workers: Workers {
            limit: state.store.settings()?.max_workers,
            busy: state.slots.busy(),
            now: now_secs(),
        },
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
        &data.workers,
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

async fn workers_fragment(
    State(state): State<AppState>,
    Query(query): Query<IndexQuery>,
) -> Result<Markup, AppError> {
    let data = load_board(&state, query.project)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {});
    };
    Ok(pages::workers(&data.workers, &data.jobs, project))
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
    events: Vec<crate::store::JobEvent>,
    cost_usd: f64,
    models: StageModels,
    efforts: StageEfforts,
    auto_merge: bool,
}

fn load_job(state: &AppState, id: i64) -> Result<JobData, AppError> {
    let row = state.store.get(id)?;
    let project = state.projects.lookup(&row.project)?;
    let settings = state.store.settings()?;
    let logs = state.store.logs(id, LOG_LINES)?;
    let events = state.store.events(id)?;
    let cost_usd = state.store.cost_usd(id)?;
    let defaults = project
        .as_ref()
        .map(|project| project.models.clone())
        .unwrap_or_default();
    let models = state.store.project_models(&row.project, &defaults)?;
    let efforts = state.store.project_efforts(&row.project)?;
    let default_auto_merge = project.as_ref().is_some_and(|p| p.auto_merge);
    let auto_merge = state
        .store
        .project_auto_merge(&row.project, default_auto_merge)?;
    Ok(JobData {
        row,
        project,
        settings,
        logs,
        events,
        cost_usd,
        models,
        efforts,
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
        &data.events,
        &images,
        data.cost_usd,
        &data.models,
        &data.efforts,
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
        &data.events,
        data.cost_usd,
        &data.models,
        &data.efforts,
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

/// `/settings` opens the first project's page, or Workers when no project is
/// onboarded.
async fn settings_home(State(state): State<AppState>) -> Result<Redirect, AppError> {
    let names = state.projects.names()?;
    Ok(match names.first() {
        Some(name) => Redirect::to(&format!("/settings/projects/{}", query_escape(name))),
        None => Redirect::to("/settings/workers"),
    })
}

fn settings_nav(state: &AppState) -> Result<pages::SettingsNav, StoreError> {
    Ok(pages::SettingsNav {
        projects: state.projects.names()?,
        servers: state
            .store
            .mcp_servers()?
            .into_iter()
            .map(|server| server.name)
            .collect(),
    })
}

fn workers_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    Ok(pages::settings_workers(
        &user(headers),
        &settings_nav(state)?,
        &state.store.settings()?,
        notice,
    ))
}

fn gates_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    Ok(pages::settings_gates(
        &user(headers),
        &settings_nav(state)?,
        &state.store.settings()?,
        notice,
    ))
}

fn github_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    Ok(pages::settings_github(
        &user(headers),
        &settings_nav(state)?,
        &state.store.settings()?,
        notice,
    ))
}

fn claude_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    // With the store unavailable the token cannot be read, so the field shows
    // "not set" and is disabled. A decrypt error also reads as not set; a save
    // then replaces it.
    let claude_token_set =
        state.secrets.available() && state.secrets.claude_token().ok().flatten().is_some();
    Ok(pages::settings_claude(
        &user(headers),
        &settings_nav(state)?,
        claude_token_set,
        state.secrets.available(),
        notice,
    ))
}

fn mcp_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    Ok(pages::settings_mcp(
        &user(headers),
        &settings_nav(state)?,
        &state.store.mcp_servers()?,
        state.secrets.available(),
        notice,
    ))
}

fn mcp_server_page(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    mcp_server_page_with(state, headers, name, notice, None, None)
}

/// A server's page, with a held-back edit or a problem to show when given.
fn mcp_server_page_with(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    notice: Option<&str>,
    pending: Option<&pages::PendingEdit>,
    problem: Option<&str>,
) -> Result<Markup, AppError> {
    let Some(server) = state.store.mcp_server(name)? else {
        return Err(AppError::NotFound(format!("no MCP server named {name:?}")));
    };
    let usage = state
        .projects
        .names()?
        .into_iter()
        .map(|project| {
            let settings = server_settings(state, &project, server.clone())?;
            Ok(pages::ServerUsage {
                project,
                enabled: settings.enabled,
                stages: settings.stages,
                headers: settings.headers,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(pages::settings_mcp_server(
        &user(headers),
        &settings_nav(state)?,
        &pages::ServerPage {
            server: &server,
            usage: &usage,
            pending,
            problem,
        },
        notice,
    ))
}

fn project_page(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    selected: pages::PipelinePart,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    let Some(project) = state.projects.lookup(name)? else {
        return Err(AppError::UnknownProject(name.to_owned()));
    };
    Ok(pages::settings_project(
        &user(headers),
        &settings_nav(state)?,
        &project_settings(state, name, &project)?,
        selected,
        notice,
    ))
}

/// The `?stage=` value of a project's settings page: the stage, gate or merge
/// whose panel is open.
#[derive(Deserialize)]
struct StageQuery {
    stage: Option<String>,
}

impl StageQuery {
    fn part(&self) -> pages::PipelinePart {
        pages::PipelinePart::parse(self.stage.as_deref())
    }
}

/// Everything a project's settings page shows: its models, efforts, flags and
/// gates, and its settings for each registered MCP server.
fn project_settings(
    state: &AppState,
    name: &str,
    project: &Project,
) -> Result<ProjectSettings, StoreError> {
    let servers = state
        .store
        .mcp_servers()?
        .into_iter()
        .map(|server| server_settings(state, name, server))
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(ProjectSettings {
        name: name.to_owned(),
        models: state.store.project_models(name, &project.models)?,
        efforts: state.store.project_efforts(name)?,
        auto_merge: state.store.project_auto_merge(name, project.auto_merge)?,
        llm_review: state.store.project_llm_review(name, project.llm_review)?,
        gates: state.store.project_gates(name)?,
        default_gates: state.store.settings()?.gates,
        servers,
    })
}

async fn show_workers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    workers_page(&state, &headers, None)
}

async fn show_gates(State(state): State<AppState>, headers: HeaderMap) -> Result<Markup, AppError> {
    gates_page(&state, &headers, None)
}

async fn show_github(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    github_page(&state, &headers, None)
}

async fn show_claude(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    claude_page(&state, &headers, None)
}

async fn show_mcp(State(state): State<AppState>, headers: HeaderMap) -> Result<Markup, AppError> {
    mcp_page(&state, &headers, None)
}

async fn show_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Markup, AppError> {
    mcp_server_page(&state, &headers, &name, None)
}

async fn show_project_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<StageQuery>,
) -> Result<Markup, AppError> {
    project_page(&state, &headers, &name, query.part(), None)
}

fn default_effort() -> String {
    factory_core::DEFAULT_EFFORT.to_owned()
}

/// A project form's fields, parsed from the raw pairs. The MCP blocks name one
/// field per server, so the set of names is not known at compile time; the
/// pairs are read by name instead of a typed struct.
struct ProjectForm(HashMap<String, String>);

impl ProjectForm {
    /// The value of `key`, or `None` when the field is absent. A text field is
    /// present with its typed value; an unticked checkbox is absent.
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Whether a checkbox named `key` was ticked.
    fn ticked(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// A model field: its trimmed value, or `None` when empty, so an empty
    /// field clears the stage's model.
    fn model(&self, key: &str) -> Option<String> {
        self.get(key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    /// An effort field, or the default when the field is absent, so a form
    /// posted before the selector existed still saves.
    fn effort(&self, key: &str) -> String {
        self.get(key).map_or_else(default_effort, str::to_owned)
    }
}

/// Saves the gate checkboxes of a project form. The form posts a hidden
/// `gates` field with them. A form without it, such as one rendered before the
/// checkboxes existed, leaves the project's gates as they are instead of
/// turning them all off.
fn save_posted_gates(store: &Store, project: &str, form: &ProjectForm) -> Result<(), StoreError> {
    if !form.ticked("gates") {
        return Ok(());
    }
    store.save_project_gates(
        project,
        Gates {
            spec: form.ticked("gate_spec"),
            plan: form.ticked("gate_plan"),
            publish: form.ticked("gate_publish"),
        },
    )
}

/// Saves the MCP stage switches of a project's pipeline. The form posts a
/// hidden `mcp.<server>.pipeline` field for each server it shows, and a server
/// without one is left as it is. A switch on turns the server on in that
/// stage, and the server runs in exactly the stages switched on. With every
/// switch off a running server stops, and keeps its stages for the server's
/// own page. A server that is off and stays off is not written.
fn save_pipeline_mcp(state: &AppState, project: &str, form: &ProjectForm) -> Result<(), AppError> {
    for server in state.store.mcp_servers()? {
        let base = format!("mcp.{}", server.name);
        if !form.ticked(&format!("{base}.pipeline")) {
            continue;
        }
        let posted = McpStages {
            clarify: form.ticked(&format!("{base}.stage.clarify")),
            spec: form.ticked(&format!("{base}.stage.spec")),
            plan: form.ticked(&format!("{base}.stage.plan")),
            implement: form.ticked(&format!("{base}.stage.implement")),
            review: form.ticked(&format!("{base}.stage.review")),
        };
        let stored = state.store.project_mcp(project, &server.name)?;
        let settings = if posted.any() {
            McpProjectSettings {
                enabled: true,
                stages: posted,
            }
        } else if stored.enabled {
            McpProjectSettings {
                enabled: false,
                stages: stored.stages,
            }
        } else {
            continue;
        };
        state
            .store
            .save_project_mcp(project, &server.name, &settings)?;
    }
    Ok(())
}

async fn save_project_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<StageQuery>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Markup, AppError> {
    if state.projects.lookup(&name)?.is_none() {
        return Err(AppError::UnknownProject(name));
    }
    let form = ProjectForm(pairs.into_iter().collect());
    let models = StageModels {
        clarify: form.model("clarify"),
        spec: form.model("spec"),
        plan: form.model("plan"),
        implement: form.model("implement"),
        review: form.model("review"),
    };
    let efforts = StageEfforts {
        clarify: form.effort("clarify_effort"),
        spec: form.effort("spec_effort"),
        plan: form.effort("plan_effort"),
        implement: form.effort("implement_effort"),
        review: form.effort("review_effort"),
    };

    // Efforts first, so a bad level is rejected before anything is written.
    state.store.save_project_efforts(&name, &efforts)?;
    state.store.save_project_models(&name, &models)?;
    state.store.save_project_settings(
        &name,
        form.ticked("auto_merge"),
        form.ticked("llm_review"),
    )?;
    save_posted_gates(&state.store, &name, &form)?;
    save_pipeline_mcp(&state, &name, &form)?;
    state.wake.notify_one();
    project_page(&state, &headers, &name, query.part(), Some("Saved."))
}

/// One server's settings as a project's page for it shows them.
fn server_settings(
    state: &AppState,
    project: &str,
    server: McpServer,
) -> Result<ServerSettings, StoreError> {
    let saved = state.store.project_mcp(project, &server.name)?;
    let headers = server
        .headers
        .iter()
        .map(|header: &String| {
            let at = SecretRef {
                project,
                server: &server.name,
                header,
            };
            Ok(HeaderState {
                name: header.clone(),
                set: state.store.has_secret(at)?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(ServerSettings {
        tools: state.store.mcp_tools(project, &server.name)?,
        name: server.name,
        enabled: saved.enabled,
        stages: saved.stages,
        headers,
    })
}

/// The project and server named in a project's MCP route, or a 404.
fn project_and_server(
    state: &AppState,
    project: &str,
    server: &str,
) -> Result<McpServer, AppError> {
    if state.projects.lookup(project)?.is_none() {
        return Err(AppError::UnknownProject(project.to_owned()));
    }
    state
        .store
        .mcp_server(server)?
        .ok_or_else(|| AppError::NotFound(format!("no MCP server named {server:?}")))
}

fn project_mcp_page(
    state: &AppState,
    headers: &HeaderMap,
    project: &str,
    server: &str,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    let registered = project_and_server(state, project, server)?;
    Ok(pages::settings_project_mcp(
        &user(headers),
        &settings_nav(state)?,
        project,
        &server_settings(state, project, registered)?,
        state.secrets.available(),
        notice,
    ))
}

async fn show_project_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, server)): Path<(String, String)>,
) -> Result<Markup, AppError> {
    project_mcp_page(&state, &headers, &project, &server, None)
}

/// Saves a project's settings for one server: whether it runs, its stages, its
/// header values and its tools. Everything is checked before any write, so a
/// rejected submit changes nothing. An enabled server needs a stage, and a
/// header value cannot be set while the secret store is unavailable.
async fn save_project_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, server)): Path<(String, String)>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Markup, AppError> {
    let registered = project_and_server(&state, &project, &server)?;
    let form = ProjectForm(pairs.into_iter().collect());
    let base = format!("mcp.{}", registered.name);
    let settings = McpProjectSettings {
        enabled: form.ticked(&format!("{base}.enabled")),
        stages: McpStages {
            clarify: form.ticked(&format!("{base}.stage.clarify")),
            spec: form.ticked(&format!("{base}.stage.spec")),
            plan: form.ticked(&format!("{base}.stage.plan")),
            implement: form.ticked(&format!("{base}.stage.implement")),
            review: form.ticked(&format!("{base}.stage.review")),
        },
    };
    if settings.enabled && !settings.stages.any() {
        return Err(McpError::NoStage(registered.name).into());
    }
    let value = |header: &str| {
        form.get(&format!("{base}.header.{header}"))
            .filter(|value| !value.is_empty())
    };
    if !state.secrets.available()
        && registered
            .headers
            .iter()
            .any(|header| value(header).is_some())
    {
        return Err(AppError::BadRequest(
            "the secret store is unavailable, so a header value cannot be saved".into(),
        ));
    }
    // Each shown tool posts a hidden `shown` field and, when it is on, a ticked
    // `tool` field. A tool the form did not show is left as it is.
    let shown = format!("{base}.shown.");
    let tools: Vec<(String, bool)> = form
        .0
        .keys()
        .filter_map(|key| key.strip_prefix(&shown))
        .map(|tool| {
            let off = !form.ticked(&format!("{base}.tool.{tool}"));
            (tool.to_owned(), off)
        })
        .collect();

    state
        .store
        .save_project_mcp(&project, &registered.name, &settings)?;
    state
        .store
        .save_mcp_tool_states(&project, &registered.name, &tools)?;
    for header in &registered.headers {
        let at = SecretRef {
            project: &project,
            server: &registered.name,
            header,
        };
        if form.ticked(&format!("{base}.clear.{header}")) {
            state.secrets.delete(at)?;
        } else if let Some(value) = value(header) {
            state.secrets.put(at, value)?;
        }
    }
    state.wake.notify_one();
    project_mcp_page(&state, &headers, &project, &server, Some("Saved."))
}

/// An MCP server form: its `url` and one `header` field per header-name row. The
/// rows post as a repeated `header` field, so the form parses as a list of pairs
/// rather than a struct. The Add form also posts a `name`; the edit route takes
/// the name from its path and ignores any posted one.
struct McpServerForm {
    name: String,
    url: String,
    headers: Vec<String>,
    /// Set when the operator confirmed an edit that deletes stored values.
    confirm: bool,
}

impl McpServerForm {
    /// Reads `name`, `url` and the repeated `header` field out of the posted
    /// pairs. Each header name is trimmed and an empty one, from a blank row, is
    /// dropped. The store validates and deduplicates the names.
    fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        let mut form = McpServerForm {
            name: String::new(),
            url: String::new(),
            headers: Vec::new(),
            confirm: false,
        };
        for (key, value) in pairs {
            match key.as_str() {
                "name" => form.name = value,
                "url" => form.url = value,
                "confirm" => form.confirm = true,
                "header" => {
                    let name = value.trim();
                    if !name.is_empty() {
                        form.headers.push(name.to_owned());
                    }
                }
                _ => {}
            }
        }
        form
    }
}

async fn add_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Markup, AppError> {
    let form = McpServerForm::from_pairs(pairs);
    state
        .store
        .create_mcp_server(form.name.trim(), form.url.trim(), &form.headers)?;
    mcp_server_page(&state, &headers, form.name.trim(), Some("Added."))
}

async fn edit_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Markup, AppError> {
    let form = McpServerForm::from_pairs(pairs);
    let url = form.url.trim();
    // An edit that deletes stored values waits for the operator to confirm it.
    let deletes = state.store.mcp_edit_deletes(&name, url, &form.headers)?;
    if !deletes.is_empty() && !form.confirm {
        let pending = pages::PendingEdit {
            url: url.to_owned(),
            headers: form.headers,
            deletes,
        };
        return mcp_server_page_with(&state, &headers, &name, None, Some(&pending), None);
    }
    state.store.edit_mcp_server(&name, url, &form.headers)?;
    mcp_server_page(&state, &headers, &name, Some("Saved."))
}

#[derive(Deserialize)]
struct DeleteServerForm {
    #[serde(default)]
    confirm_name: String,
}

/// Deletes a server once the operator typed its name. Any other name redraws
/// the server's page with the problem and deletes nothing.
async fn delete_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<DeleteServerForm>,
) -> Result<Markup, AppError> {
    if form.confirm_name.trim() != name {
        let problem = format!("Nothing was deleted. Type {name} to delete the server.");
        return mcp_server_page_with(&state, &headers, &name, None, None, Some(&problem));
    }
    state.store.delete_mcp_server(&name)?;
    mcp_page(&state, &headers, Some("Deleted."))
}

/// Checks a project's connection to one MCP server, stores the tools it lists,
/// and returns the server block's refresh fragment: the result and the tool
/// list, which htmx swaps in. The check uses the saved URL and the project's
/// saved header values, resolved the way the proxy resolves them, so it and a
/// real run send the same headers. Any form field posted with the request is
/// ignored, so an unsaved value typed into the page is never sent. Only a
/// complete tool list replaces the stored one; the result itself is not
/// stored.
async fn check_mcp_server(
    State(state): State<AppState>,
    Path((project, server)): Path<(String, String)>,
) -> Result<Markup, AppError> {
    if state.projects.lookup(&project)?.is_none() {
        return Err(AppError::UnknownProject(project));
    }
    let Some(registered) = state.store.mcp_server(&server)? else {
        return Err(AppError::NotFound(format!(
            "no MCP server named {server:?}"
        )));
    };
    // Resolve the saved URL and values the way the proxy does, so the check and
    // a run send the same headers. `resolve` returns `None` only for a server
    // that is not registered, which the lookup above has ruled out.
    let upstreams = StoreUpstreams::new(Arc::clone(&state.store), Arc::clone(&state.secrets));
    let Upstream { url, headers, .. } = upstreams
        .resolve(&project, &server)
        .ok_or_else(|| AppError::NotFound(format!("no MCP server named {server:?}")))?;
    // A declared header with no resolved value has none stored, or one that
    // would not decrypt. The result names these, and the check runs without
    // them.
    let missing: Vec<String> = registered
        .headers
        .iter()
        .filter(|name| !headers.iter().any(|(have, _)| have == *name))
        .cloned()
        .collect();
    let outcome = state.checker.check(&url, headers).await;
    if let CheckOutcome::Ok {
        tools: ToolList::Listed(tools),
        ..
    } = &outcome
    {
        state.store.replace_mcp_tools(&project, &server, tools)?;
    }
    let tools = state.store.mcp_tools(&project, &server)?;
    Ok(pages::mcp_refresh(
        &project,
        &server,
        &tools,
        Some(pages::mcp_check_result(&outcome, &missing)),
    ))
}

#[derive(Deserialize)]
struct WorkersForm {
    max_workers: usize,
    worker_image: String,
    worker_memory: String,
    worker_cpus: String,
}

#[derive(Deserialize)]
struct GatesForm {
    /// Checkboxes: present when ticked.
    #[serde(rename = "gate_spec")]
    spec: Option<String>,
    #[serde(rename = "gate_plan")]
    plan: Option<String>,
    #[serde(rename = "gate_publish")]
    publish: Option<String>,
}

#[derive(Deserialize)]
struct GithubForm {
    poll_seconds: u64,
}

/// A trimmed text field, or a 400 that names the field when it is empty.
fn required(value: &str, name: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() {
        Err(AppError::BadRequest(format!("{name} is empty")))
    } else {
        Ok(value.to_owned())
    }
}

/// Each settings page saves its own fields into the stored settings and leaves
/// the rest as they are.
async fn save_workers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<WorkersForm>,
) -> Result<Markup, AppError> {
    let settings = Settings {
        max_workers: form.max_workers,
        worker_image: required(&form.worker_image, "the worker image")?,
        worker_memory: required(&form.worker_memory, "the worker memory")?,
        worker_cpus: required(&form.worker_cpus, "the worker CPUs")?,
        ..state.store.settings()?
    };
    state.store.save_settings(&settings)?;
    state.wake.notify_one();
    workers_page(&state, &headers, Some("Saved."))
}

async fn save_gates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<GatesForm>,
) -> Result<Markup, AppError> {
    let settings = Settings {
        gates: Gates {
            spec: form.spec.is_some(),
            plan: form.plan.is_some(),
            publish: form.publish.is_some(),
        },
        ..state.store.settings()?
    };
    state.store.save_settings(&settings)?;
    gates_page(&state, &headers, Some("Saved."))
}

async fn save_github(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<GithubForm>,
) -> Result<Markup, AppError> {
    if form.poll_seconds < MIN_POLL_SECONDS {
        return Err(AppError::BadRequest(format!(
            "the poll interval must be at least {MIN_POLL_SECONDS} seconds"
        )));
    }
    let settings = Settings {
        poll_seconds: form.poll_seconds,
        ..state.store.settings()?
    };
    state.store.save_settings(&settings)?;
    state.wake.notify_one();
    github_page(&state, &headers, Some("Saved."))
}

#[derive(Deserialize)]
struct ClaudeTokenForm {
    #[serde(default)]
    claude_token: String,
}

/// Replaces the stored Claude token with the posted value. The value is trimmed
/// of surrounding whitespace and a trailing newline, the way a pasted
/// `claude setup-token` output needs. An empty field keeps the stored token, so
/// a save that leaves the field blank is a no-op. The resolver reads the token
/// afresh, so the next worker run and the next plan usage refresh use the new
/// one with no restart.
async fn save_claude_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ClaudeTokenForm>,
) -> Result<Markup, AppError> {
    let token = form.claude_token.trim();
    if !token.is_empty() {
        state.secrets.set_claude_token(token)?;
    }
    claude_page(&state, &headers, Some("Saved."))
}

/// Clears the stored Claude token. The posted form fields, if any, are ignored.
/// Clearing works even when the master key is missing, so a token stranded by a
/// lost key can still be removed.
async fn clear_claude_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    state.secrets.clear_claude_token()?;
    claude_page(&state, &headers, Some("Cleared."))
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
