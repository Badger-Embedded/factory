use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::multipart::{Field, Multipart, MultipartError};
use axum::extract::{DefaultBodyLimit, Form, Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use factory_core::{Agent, Command, DEFAULT_AGENT, Event, Gates, Stage};
use factory_runner::Expect;
use maud::{Markup, html};
use serde::Deserialize;
use tokio::sync::Notify;

use crate::check::{CheckOutcome, McpChecker, ToolList};
use crate::copilot::validate_copilot_token;
use crate::device_login::DeviceLogin;
use crate::github::{RepoChecker, RepoReport};
use crate::mcp::{McpError, McpProjectSettings, McpServer, McpStages};
use crate::pages::{self, HeaderState, ProjectSettings, ServerSettings, Workers};
use crate::projects::{Project, Projects, StageAgents, StageEfforts, StageModels};
use crate::proxy::{StoreUpstreams, Upstream, Upstreams};
use crate::registry::AgentRegistry;
use crate::secrets::{SecretError, SecretRef, SecretStore, validate_codex_auth};
use crate::settings::Settings;
use crate::store::{
    FACTORY_REQUESTER, JobFile, JobRow, Maintenance, Packaging, PackagingType, ProjectTotals,
    Store, StoreError,
};
use crate::usage::PlanUsageCache;
use crate::workers::WorkerSlots;

/// The header `tailscale serve` sets to the logged-in user's login name.
pub const USER_HEADER: &str = "Tailscale-User-Login";

const LOCAL_USER: &str = "local";
const LOG_LINES: usize = 300;
/// GitHub allows 5000 API calls an hour, and each look at a pull request
/// takes two.
const MIN_POLL_SECONDS: u64 = 10;

/// Attachments one request may carry, images and files counted together.
pub const MAX_ATTACHMENTS: usize = 5;
/// Bytes in one attachment.
pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// Bytes of the request text and the other form fields together.
const MAX_FORM_BYTES: usize = 256 * 1024;
const MAX_UPLOAD_BYTES: usize = MAX_ATTACHMENTS * MAX_ATTACHMENT_BYTES + MAX_FORM_BYTES;

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
    registry: Arc<AgentRegistry>,
    device_login: Arc<DeviceLogin>,
    onboarding: Arc<tokio::sync::Mutex<()>>,
    copilot_login: Arc<DeviceLogin>,
    admins: Arc<Vec<String>>,
}

/// The dashboard. Every change it makes to a job notifies `wake`, so the
/// job loop looks at the job right away. Uploaded images go under
/// `<data_dir>/jobs/<id>/images/`. The index page reads `plan_usage` from the
/// cache the worker runs fill.
/// The onboarding wizard runs its repository checks through `github`. The
/// board shows the worker slots the scheduler fills in `slots`. Per-project
/// MCP header values are read and written through `secrets`, which reports
/// itself unavailable when no master key is present. The per-project MCP
/// connection check runs through `checker`, the way the onboarding checks run
/// through `github`. The settings pages list the agents and link their account
/// pages through `registry`.
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
    registry: Arc<AgentRegistry>,
    device_login: Arc<DeviceLogin>,
    copilot_login: Arc<DeviceLogin>,
    admins: Vec<String>,
) -> Router {
    let state = AppState {
        store,
        projects,
        wake,
        data_dir,
        plan_usage,
        github,
        slots,
        secrets,
        checker,
        registry,
        device_login,
        onboarding: Arc::new(tokio::sync::Mutex::new(())),
        copilot_login,
        admins: Arc::new(admins),
    };
    Router::new()
        .route("/", get(index))
        .route("/notifications", get(notifications))
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
        .route(
            "/jobs/{id}/edit",
            get(show_edit)
                .post(save_edit)
                .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/jobs/{id}/fragment", get(job_fragment))
        .route("/jobs/{id}/images/{name}", get(show_image))
        .route("/jobs/{id}/files/{name}", get(show_file))
        .route("/jobs/{id}/answers", post(answer))
        .route("/jobs/{id}/approve", post(approve))
        .route("/jobs/{id}/retry", post(retry))
        .route("/jobs/{id}/dismiss", post(dismiss))
        .route("/jobs/{id}/changes", post(request_changes))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/settings", get(settings_home))
        .route(
            "/settings/notifications",
            get(show_notifications).post(save_notifications),
        )
        .route(
            "/settings/projects/{name}",
            get(show_project_settings).post(save_project_models),
        )
        .route(
            "/settings/projects/{project}/collaborators",
            post(add_project_collaborator),
        )
        .route(
            "/settings/projects/{project}/collaborators/remove",
            post(remove_project_collaborator),
        )
        .route(
            "/settings/projects/{project}/maintenance",
            post(save_project_maintenance),
        )
        .route("/settings/projects/{project}/remove", post(remove_project))
        .route(
            "/settings/projects/{project}/mcp/{server}",
            get(show_project_mcp).post(save_project_mcp),
        )
        .route(
            "/settings/projects/{project}/mcp/{server}/check",
            post(check_mcp_server),
        )
        .merge(admin_routes(state.clone()))
        .with_state(state)
}

fn admin_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/settings/workers", get(show_workers).post(save_workers))
        .route("/settings/gates", get(show_gates).post(save_gates))
        .route("/settings/github", get(show_github).post(save_github))
        .route("/settings/renovate", get(show_renovate).post(save_renovate))
        .route("/settings/claude", get(redirect_claude))
        .route("/settings/agents/claude", get(show_claude))
        .route("/settings/agents/claude/token", post(save_claude_token))
        .route(
            "/settings/agents/claude/token/clear",
            post(clear_claude_token),
        )
        .route("/settings/agents/codex", get(show_codex))
        .route("/settings/agents/codex/auth", post(save_codex_auth))
        .route("/settings/agents/codex/auth/clear", post(clear_codex_auth))
        .route("/settings/agents/codex/login", post(start_codex_login))
        .route(
            "/settings/agents/codex/login/fragment",
            get(codex_login_fragment),
        )
        .route(
            "/settings/agents/codex/login/cancel",
            post(cancel_codex_login),
        )
        .route("/settings/agents/codex/prices", post(save_codex_price))
        .route(
            "/settings/agents/codex/prices/delete",
            post(delete_codex_price),
        )
        .route("/settings/mcp", get(show_mcp).post(add_mcp_server))
        .route(
            "/settings/mcp/{name}",
            get(show_mcp_server).post(edit_mcp_server),
        )
        .route("/settings/mcp/{name}/delete", post(delete_mcp_server))
        .merge(copilot_routes())
        .route_layer(middleware::from_fn_with_state(state, require_admin))
}

/// The admin list used when `FACTORY_ADMINS` is unset.
pub const DEFAULT_ADMINS: &str = "talhahavadar@hotmail.com";

/// Parses the `FACTORY_ADMINS` value: comma-separated logins, whitespace
/// trimmed, empty entries dropped. `None` gives `DEFAULT_ADMINS`.
pub fn parse_admins(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or(DEFAULT_ADMINS)
        .split(',')
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn require_admin(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if state.admins.contains(&user(request.headers())) {
        return next.run(request).await;
    }
    let status = StatusCode::FORBIDDEN;
    (
        status,
        pages::error(status, "This page is for factory admins."),
    )
        .into_response()
}

fn copilot_routes() -> Router<AppState> {
    Router::new()
        .route("/settings/agents/copilot", get(show_copilot))
        .route("/settings/agents/copilot/token", post(save_copilot_token))
        .route(
            "/settings/agents/copilot/token/clear",
            post(clear_copilot_token),
        )
        .route("/settings/agents/copilot/login", post(start_copilot_login))
        .route(
            "/settings/agents/copilot/login/fragment",
            get(copilot_login_fragment),
        )
        .route(
            "/settings/agents/copilot/login/cancel",
            post(cancel_copilot_login),
        )
}

fn user(headers: &HeaderMap) -> String {
    headers
        .get(USER_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|login| !login.is_empty())
        .unwrap_or(LOCAL_USER)
        .to_owned()
}

/// The project named in a route when the request's login is one of its
/// collaborators. A project that does not exist, and one the login is not a
/// collaborator on, both return [`AppError::UnknownProject`], so a project a
/// login may not see is indistinguishable from one that is not there.
fn visible_project(state: &AppState, headers: &HeaderMap, name: &str) -> Result<Project, AppError> {
    let login = user(headers);
    match state.projects.lookup(name)? {
        Some(project) if state.store.is_collaborator(name, &login)? => Ok(project),
        _ => Err(AppError::UnknownProject(name.to_owned())),
    }
}

/// The job with `id` when the request's login is a collaborator on the job's
/// project. An unknown job, and a job in a project the login may not see, both
/// return [`StoreError::NotFound`], so a job a login may not see looks exactly
/// like one that is not there.
fn visible_job(state: &AppState, headers: &HeaderMap, id: i64) -> Result<JobRow, AppError> {
    let login = user(headers);
    let row = state.store.get(id)?;
    if state.store.is_collaborator(&row.project, &login)? {
        Ok(row)
    } else {
        Err(AppError::from(StoreError::NotFound(id)))
    }
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
            AppError::Store(err @ (StoreError::InvalidEffort(_) | StoreError::InvalidAgent(_))) => {
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
    /// The version of the region a poll already shows. The handler answers 204
    /// when it still matches, so htmx leaves the region's DOM alone. Absent on
    /// the full-page load and on a stale poll.
    #[serde(default)]
    v: Option<String>,
}

/// A region poll's answer: 204 when the poll's version still matches what the
/// server would render, so htmx does not swap, or the region's markup when it
/// changed or the poll carried no version.
fn region_response(requested: Option<&str>, region: pages::Region) -> Response {
    if requested == Some(region.version.as_str()) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        region.markup.into_response()
    }
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

fn load_board(
    state: &AppState,
    login: &str,
    project: Option<String>,
) -> Result<BoardData, AppError> {
    let names = state.store.projects_for(login)?;
    let selected = match project {
        Some(name) if !names.iter().any(|known| known == &name) => {
            return Err(AppError::UnknownProject(name));
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
    let login = user(&headers);
    let data = load_board(&state, &login, query.project)?;
    let plan_usage = usage_tile(&state);
    let notifications = state.store.notifications_enabled(&login)?;
    Ok(pages::index(
        &login,
        &data.names,
        data.selected.as_deref(),
        &data.jobs,
        &data.costs,
        data.totals,
        plan_usage.as_ref(),
        data.auto_merge,
        &data.workers,
        notifications,
    ))
}

async fn notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, AppError> {
    let login = user(&headers);
    if !state.store.notifications_enabled(&login)? {
        return Ok(axum::Json(
            serde_json::json!({ "enabled": false, "items": [] }),
        ));
    }
    let mut auto_merge = HashMap::new();
    for name in state.store.projects_for(&login)? {
        let default = state.projects.lookup(&name)?.is_some_and(|p| p.auto_merge);
        auto_merge.insert(
            name.clone(),
            state.store.project_auto_merge(&name, default)?,
        );
    }
    let snapshot = state.store.list_with_latest_event()?;
    let items: Vec<_> = snapshot
        .jobs
        .iter()
        .filter(|(row, _)| row.requested_by == login || row.requested_by == FACTORY_REQUESTER)
        .filter_map(|(row, event)| {
            let merge = *auto_merge.get(&row.project)?;
            let kind = pages::attention(&row.job, merge)?;
            Some(serde_json::json!({
                "key": format!("{}:{event}", row.id),
                "job": row.id,
                "title": pages::job_title(&row.job),
                "kind": kind,
                "url": format!("/jobs/{}", row.id),
            }))
        })
        .collect();
    Ok(axum::Json(serde_json::json!({
        "enabled": true,
        "snapshot": [snapshot.order.0, snapshot.order.1],
        "items": items,
    })))
}

/// Serves the plan usage tile on its own, from the same cache `index` reads. A
/// poll whose version still matches gets a 204, so the tile does not redraw.
async fn plan_usage_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let names = state.store.projects_for(&user(&headers))?;
    if let Some(project) = &query.project
        && !names.iter().any(|known| known == project)
    {
        return Err(AppError::UnknownProject(project.clone()));
    }
    if names.is_empty() {
        return Ok(html! {}.into_response());
    }
    let region = pages::plan_usage(usage_tile(&state).as_ref());
    Ok(region_response(query.v.as_deref(), region))
}

/// The plan usage tile, or `None` before any run has reported usage.
fn usage_tile(state: &AppState) -> Option<pages::UsageTile> {
    state
        .plan_usage
        .snapshot()
        .map(|snapshot| pages::UsageTile {
            snapshot,
            now: now_secs(),
        })
}

async fn stats_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let version = query.v.clone();
    let data = load_board(&state, &user(&headers), query.project)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {}.into_response());
    };
    Ok(region_response(
        version.as_deref(),
        pages::stats(data.totals, project),
    ))
}

async fn workers_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let version = query.v.clone();
    let data = load_board(&state, &user(&headers), query.project)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {}.into_response());
    };
    Ok(region_response(
        version.as_deref(),
        pages::workers(&data.workers, &data.jobs, &data.names, project),
    ))
}

async fn board_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let version = query.v.clone();
    let data = load_board(&state, &user(&headers), query.project)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {}.into_response());
    };
    Ok(region_response(
        version.as_deref(),
        pages::board(&data.rows(), &data.costs, data.auto_merge, project),
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

/// The accepted upload kinds, named in the 400 a rejected upload gets.
const ACCEPTED_TYPES: &str = "an upload is not an accepted type: a PNG, JPEG, GIF or WebP image, a UTF-8 text file, \
     or a ZIP archive";

/// One classified attachment. An image keeps the current storage; a file is a
/// UTF-8 text file or a ZIP archive. `name` on a file is the requester's file
/// name, trimmed to its last path component, or `None` when the upload carried
/// no usable name.
enum Attachment {
    Image {
        extension: &'static str,
        bytes: Vec<u8>,
    },
    File {
        extension: &'static str,
        name: Option<String>,
        bytes: Vec<u8>,
    },
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

/// Whether `bytes` are a ZIP archive: an archive with entries starts with
/// `PK\x03\x04`, and an empty archive with `PK\x05\x06`.
fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06")
}

/// Whether `bytes` are a UTF-8 text file: valid UTF-8, which allows a leading
/// byte order mark, and with no NUL byte.
fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Keeps the last path component of an upload's file name, since some browsers
/// send a full path, and cuts the result to 255 characters. An empty result,
/// such as from a name that is all slashes, becomes `None`.
fn attachment_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let trimmed: String = last.chars().take(255).collect();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Classifies one upload's bytes, checked in order: image signatures, then a
/// ZIP signature, then UTF-8 text. Anything else, including zero bytes, is a
/// 400 that names the accepted types.
fn classify(name: Option<String>, bytes: Vec<u8>) -> Result<Attachment, AppError> {
    if bytes.is_empty() {
        return Err(AppError::BadRequest(ACCEPTED_TYPES.into()));
    }
    if let Some(extension) = image_extension(&bytes) {
        Ok(Attachment::Image { extension, bytes })
    } else if is_zip(&bytes) {
        Ok(Attachment::File {
            extension: "zip",
            name,
            bytes,
        })
    } else if is_text(&bytes) {
        Ok(Attachment::File {
            extension: "txt",
            name,
            bytes,
        })
    } else {
        Err(AppError::BadRequest(ACCEPTED_TYPES.into()))
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

/// Reads one upload part's bytes and its file name. Browsers send an empty
/// part with no file name when no file is chosen; that part yields `None`.
/// Stops reading once a part passes the per-attachment limit.
async fn read_upload(mut field: Field<'_>) -> Result<Option<(Option<String>, Vec<u8>)>, AppError> {
    let name = field
        .file_name()
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|err| multipart_error(&err))? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(AppError::BadRequest(format!(
                "an attachment is over the limit of {MAX_ATTACHMENT_BYTES} bytes"
            )));
        }
    }
    if bytes.is_empty() && name.is_none() {
        return Ok(None);
    }
    Ok(Some((name.as_deref().and_then(attachment_name), bytes)))
}

/// Classifies one upload's bytes and adds it to `attachments`.
async fn read_attachment(
    field: Field<'_>,
    attachments: &mut Vec<Attachment>,
) -> Result<(), AppError> {
    let Some((name, bytes)) = read_upload(field).await? else {
        return Ok(());
    };
    if attachments.len() == MAX_ATTACHMENTS {
        return Err(AppError::BadRequest(format!(
            "a request takes at most {MAX_ATTACHMENTS} attachments"
        )));
    }
    attachments.push(classify(name, bytes)?);
    Ok(())
}

async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    let mut project = None;
    let mut request = None;
    let mut attachments = Vec::new();
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|err| multipart_error(&err))?
    {
        match field.name() {
            Some("project") => project = Some(read_text(field).await?),
            Some("request") => request = Some(read_text(field).await?),
            Some("images") => read_attachment(field, &mut attachments).await?,
            _ => {}
        }
    }
    let project = project.unwrap_or_default();
    visible_project(&state, &headers, &project)?;
    let request = request.unwrap_or_default();
    let request = request.trim();
    if request.is_empty() {
        return Err(AppError::BadRequest("the request is empty".into()));
    }
    let id = state.store.create(&project, &user(&headers), request)?;
    if !attachments.is_empty()
        && let Err(err) = save_attachments(&state, id, &attachments)
    {
        state.store.apply(id, Event::Cancel)?;
        return Err(err);
    }
    Ok(job_redirect(id))
}

/// Writes each attachment under the job's directory with a generated name, so
/// an upload's own file name never reaches the file system, and records the
/// images in `job_images` and the files in `job_files`.
fn save_attachments(state: &AppState, id: i64, attachments: &[Attachment]) -> Result<(), AppError> {
    let job_dir = state.data_dir.join("jobs").join(id.to_string());
    let (image_names, files) = write_numbered_attachments(&job_dir, attachments)?;
    if !image_names.is_empty() {
        state.store.add_images(id, &image_names)?;
    }
    if !files.is_empty() {
        state.store.add_files(id, &files)?;
    }
    Ok(())
}

/// Writes `attachments` under `dir`, images in its `images/` subdirectory and
/// files in its `files/` subdirectory, each type numbered from 1 in order as
/// `image-<n>.<ext>` or `file-<n>.<ext>`. A file's original name is its own,
/// or its stored name when the upload carried none. Returns the image names
/// and the file rows for the caller to record.
fn write_numbered_attachments(
    dir: &std::path::Path,
    attachments: &[Attachment],
) -> Result<(Vec<String>, Vec<JobFile>), AppError> {
    let mut image_names = Vec::new();
    let mut files = Vec::new();
    for attachment in attachments {
        match attachment {
            Attachment::Image { extension, bytes } => {
                let name = format!("image-{}.{extension}", image_names.len() + 1);
                write_attachment(&dir.join("images"), &name, bytes)?;
                image_names.push(name);
            }
            Attachment::File {
                extension,
                name,
                bytes,
            } => {
                let stored = format!("file-{}.{extension}", files.len() + 1);
                write_attachment(&dir.join("files"), &stored, bytes)?;
                let original_name = name.clone().unwrap_or_else(|| stored.clone());
                files.push(JobFile {
                    name: stored,
                    original_name,
                });
            }
        }
    }
    Ok((image_names, files))
}

/// Writes one attachment's bytes under `dir`, creating `dir` on the first file.
fn write_attachment(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Result<(), AppError> {
    let io = |err: std::io::Error| AppError::Io(format!("cannot save the attachments: {err}"));
    std::fs::create_dir_all(dir).map_err(io)?;
    std::fs::write(dir.join(name), bytes).map_err(io)?;
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

fn keep_session_settings(
    state: &AppState,
    row: &JobRow,
    models: &mut StageModels,
    efforts: &mut StageEfforts,
) -> Result<(), AppError> {
    let Some(Command::RunAgent { session, task }) = row.job.next_command() else {
        return Ok(());
    };
    let Some(expect) = Expect::for_task(row.job.stage(), &task) else {
        return Ok(());
    };
    let Some(last) = state.store.session_settings(row.id, session)? else {
        return Ok(());
    };
    let agents = state.store.project_agents(&row.project)?;
    let (model, effort, stage_agent) = match expect {
        Expect::Clarify => (&mut models.clarify, &mut efforts.clarify, &agents.clarify),
        Expect::Spec => (&mut models.spec, &mut efforts.spec, &agents.spec),
        Expect::Plan => (&mut models.plan, &mut efforts.plan, &agents.plan),
        Expect::Commits | Expect::FixCi => (
            &mut models.implement,
            &mut efforts.implement,
            &agents.implement,
        ),
        Expect::Review | Expect::FailureAnalysis | Expect::DiffCheck => {
            (&mut models.review, &mut efforts.review, &agents.review)
        }
    };
    let on_stage_agent = Agent::from_id(stage_agent).unwrap_or(DEFAULT_AGENT) == last.agent;
    if on_stage_agent && !state.slots.has_task(row.id) {
        return Ok(());
    }
    *effort = last
        .effort
        .unwrap_or_else(|| last.agent.default_effort().to_owned());
    *model = last.model;
    Ok(())
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
    let mut models = state.store.project_models(&row.project, &defaults)?;
    let mut efforts = state.store.project_efforts(&row.project)?;
    keep_session_settings(state, &row, &mut models, &mut efforts)?;
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
    visible_job(&state, &headers, id)?;
    let data = load_job(&state, id)?;
    let images = state.store.images(id)?;
    let files = state.store.files(id)?;
    Ok(pages::job(
        &user(&headers),
        &data.row,
        data.project.as_ref(),
        &data.settings,
        &data.logs,
        &data.events,
        &images,
        &files,
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
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Markup, AppError> {
    visible_job(&state, &headers, id)?;
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
    headers: HeaderMap,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    let not_found = || AppError::from(StoreError::NotFound(id));
    visible_job(&state, &headers, id)?;
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

/// Serves a file the store lists for the job. The name must match a stored
/// one, so a request cannot reach any other file. A text file is served as
/// `text/plain`, a ZIP archive as `application/zip`, both as an attachment
/// whose download name is the original name, percent-encoded so the name
/// never enters the header unencoded.
async fn show_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    let not_found = || AppError::from(StoreError::NotFound(id));
    visible_job(&state, &headers, id)?;
    let content_type = match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("txt") => "text/plain; charset=utf-8",
        Some("zip") => "application/zip",
        _ => return Err(not_found()),
    };
    let Some(file) = state
        .store
        .files(id)?
        .into_iter()
        .find(|file| file.name == name)
    else {
        return Err(not_found());
    };
    let path = state
        .data_dir
        .join("jobs")
        .join(id.to_string())
        .join("files")
        .join(&name);
    let bytes = tokio::fs::read(path).await.map_err(|_| not_found())?;
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        query_escape(&file.original_name)
    );
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_DISPOSITION, disposition.as_str()),
        ],
        bytes,
    )
        .into_response())
}

/// Shows the edit form for a job still queued in `requested`. A job that has
/// already started, or any job in a later stage, redirects to the job page, so
/// the form only ever opens on an editable request.
async fn show_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let row = visible_job(&state, &headers, id)?;
    if *row.job.stage() != Stage::Requested {
        return Ok(job_redirect(id).into_response());
    }
    let images = state.store.images(id)?;
    let files = state.store.files(id)?;
    Ok(pages::edit_request(&user(&headers), id, &row.job, &images, &files).into_response())
}

/// Saves an edit to a queued request. It applies the New request form's rules
/// to the new text and uploads, rejects an unknown `remove` name, then writes
/// the kept-then-new attachment set to the job's `staging/` directory and hands
/// it to [`Store::edit_request`], which swaps it in under the connection lock.
/// A job that already started gets a 409 from the store and nothing changes.
async fn save_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
    let mut request = None;
    let mut removes: Vec<String> = Vec::new();
    let mut attachments = Vec::new();
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|err| multipart_error(&err))?
    {
        match field.name() {
            Some("request") => request = Some(read_text(field).await?),
            Some("remove") => removes.push(read_text(field).await?),
            Some("images") => read_attachment(field, &mut attachments).await?,
            _ => {}
        }
    }
    let request = request.unwrap_or_default();
    let request = request.trim();
    if request.is_empty() {
        return Err(AppError::BadRequest("the request is empty".into()));
    }
    if request.len() > MAX_FORM_BYTES {
        return Err(AppError::BadRequest(format!(
            "the request is over the limit of {MAX_FORM_BYTES} bytes"
        )));
    }

    // The kept set: the stored attachments the form did not mark for removal.
    // An unknown `remove` name is a 400, so a stale form cannot drop a file
    // that is no longer there or reach a name the job never had.
    let images = state.store.images(id)?;
    let files = state.store.files(id)?;
    let stored: HashSet<&str> = images
        .iter()
        .map(String::as_str)
        .chain(files.iter().map(|file| file.name.as_str()))
        .collect();
    for name in &removes {
        if !stored.contains(name.as_str()) {
            return Err(AppError::BadRequest(format!(
                "no attachment named {name:?}"
            )));
        }
    }
    let dropped: HashSet<&str> = removes.iter().map(String::as_str).collect();
    let kept_images: Vec<&String> = images
        .iter()
        .filter(|name| !dropped.contains(name.as_str()))
        .collect();
    let kept_files: Vec<&JobFile> = files
        .iter()
        .filter(|file| !dropped.contains(file.name.as_str()))
        .collect();
    if kept_images.len() + kept_files.len() + attachments.len() > MAX_ATTACHMENTS {
        return Err(AppError::BadRequest(format!(
            "a request takes at most {MAX_ATTACHMENTS} attachments"
        )));
    }

    // The final set reads the kept attachments' bytes, in their old order,
    // then the new uploads, so the staging write renumbers both from 1.
    let job_dir = state.data_dir.join("jobs").join(id.to_string());
    let mut combined = Vec::new();
    for name in &kept_images {
        let bytes = read_stored(&job_dir.join("images").join(name))?;
        combined.push(Attachment::Image {
            extension: image_ext(name)?,
            bytes,
        });
    }
    for file in &kept_files {
        let bytes = read_stored(&job_dir.join("files").join(&file.name))?;
        combined.push(Attachment::File {
            extension: file_ext(&file.name)?,
            name: Some(file.original_name.clone()),
            bytes,
        });
    }
    combined.extend(attachments);

    let staging_dir = job_dir.join("staging");
    let _ = std::fs::remove_dir_all(&staging_dir);
    let (image_names, new_files) = match write_numbered_attachments(&staging_dir, &combined) {
        Ok(set) => set,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(err);
        }
    };

    let result = state.store.edit_request(
        id,
        request.to_owned(),
        &state.data_dir,
        &image_names,
        &new_files,
    );
    let _ = std::fs::remove_dir_all(&staging_dir);
    result?;
    state.wake.notify_one();
    Ok(job_redirect(id))
}

/// Reads a stored attachment's bytes, mapping a read failure to a 500, since a
/// file the tables list should be on disk.
fn read_stored(path: &std::path::Path) -> Result<Vec<u8>, AppError> {
    std::fs::read(path)
        .map_err(|err| AppError::Io(format!("cannot read a saved attachment: {err}")))
}

/// The extension of a stored image name, one of the server-generated set. A
/// name the server never wrote is a 500 rather than a silent miss.
fn image_ext(name: &str) -> Result<&'static str, AppError> {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("png") => Ok("png"),
        Some("jpg") => Ok("jpg"),
        Some("gif") => Ok("gif"),
        Some("webp") => Ok("webp"),
        _ => Err(AppError::Io(format!(
            "stored image {name} has no known extension"
        ))),
    }
}

/// The extension of a stored file name, a text file or a ZIP archive.
fn file_ext(name: &str) -> Result<&'static str, AppError> {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("txt") => Ok("txt"),
        Some("zip") => Ok("zip"),
        _ => Err(AppError::Io(format!(
            "stored file {name} has no known extension"
        ))),
    }
}

async fn answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
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
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<ApproveForm>,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
    let redirect = apply(&state, id, Event::Approve)?;
    board_redirect(&state, form.project, redirect)
}

async fn retry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<ApproveForm>,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
    let redirect = apply(&state, id, Event::Retry)?;
    state
        .store
        .append_factory_log(id, "Retried the job from the dashboard.")?;
    board_redirect(&state, form.project, redirect)
}

async fn dismiss(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<ApproveForm>,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
    let redirect = apply(&state, id, Event::Dismiss)?;
    state
        .store
        .append_factory_log(id, "Dismissed the job from the dashboard.")?;
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
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<Feedback>,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
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

/// `/settings` opens the first project the login collaborates on, or onboarding
/// when the login has none.
async fn settings_home(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NoticeQuery>,
) -> Result<Redirect, AppError> {
    let names = state.store.projects_for(&user(&headers))?;
    let tail = match &query.notice {
        Some(notice) => format!("?notice={}", query_escape(notice)),
        None => String::new(),
    };
    Ok(match names.first() {
        Some(name) => Redirect::to(&format!("/settings/projects/{}{tail}", query_escape(name))),
        None => Redirect::to(&format!("/settings/notifications{tail}")),
    })
}

fn notifications_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    let login = user(headers);
    Ok(pages::settings_notifications(
        &login,
        &settings_nav(state, &login)?,
        state.store.notifications_enabled(&login)?,
        notice,
    ))
}

async fn show_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NoticeQuery>,
) -> Result<Markup, AppError> {
    notifications_page(&state, &headers, query.notice.as_deref())
}

#[derive(Deserialize)]
struct NotificationsForm {
    notifications: Option<String>,
}

async fn save_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NotificationsForm>,
) -> Result<Markup, AppError> {
    state
        .store
        .set_notifications(&user(&headers), form.notifications.is_some())?;
    notifications_page(&state, &headers, Some("Saved."))
}

#[derive(Deserialize)]
struct NoticeQuery {
    notice: Option<String>,
}

fn settings_nav(state: &AppState, login: &str) -> Result<pages::SettingsNav, StoreError> {
    Ok(pages::SettingsNav {
        admin: state.admins.iter().any(|admin| admin == login),
        projects: state.store.projects_for(login)?,
        servers: state
            .store
            .mcp_servers()?
            .into_iter()
            .map(|server| server.name)
            .collect(),
        agents: state
            .registry
            .entries()
            .map(|entry| pages::AgentNav {
                id: entry.id().to_owned(),
                name: entry.display_name().to_owned(),
                url: entry.settings_url().map(str::to_owned),
                available: entry.available(),
            })
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
        &settings_nav(state, &user(headers))?,
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
        &settings_nav(state, &user(headers))?,
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
        &settings_nav(state, &user(headers))?,
        &state.store.settings()?,
        notice,
    ))
}

fn renovate_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
    problem: Option<&str>,
) -> Result<Markup, AppError> {
    Ok(pages::settings_renovate(
        &user(headers),
        &settings_nav(state, &user(headers))?,
        &state.store.settings()?,
        notice,
        problem,
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
        &settings_nav(state, &user(headers))?,
        claude_token_set,
        state.secrets.available(),
        notice,
    ))
}

/// The Codex account page: whether a login is stored, when Codex last refreshed
/// it, and the current state of a device login. The stored file never reaches
/// the page; only the `last_refresh` time does.
async fn codex_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    let auth = state
        .secrets
        .available()
        .then(|| state.secrets.codex_auth().ok().flatten())
        .flatten();
    let account = pages::CodexAccount {
        stored: auth.is_some(),
        last_refresh: auth.as_deref().and_then(codex_last_refresh),
    };
    let login = state.device_login.snapshot().await;
    let prices = state.store.codex_prices()?;
    Ok(pages::settings_codex(
        &user(headers),
        &settings_nav(state, &user(headers))?,
        &account,
        &login,
        &prices,
        state.secrets.available(),
        notice,
    ))
}

async fn copilot_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    let stored =
        state.secrets.available() && state.secrets.copilot_token().ok().flatten().is_some();
    let login = state.copilot_login.snapshot().await;
    Ok(pages::settings_copilot(
        &user(headers),
        &settings_nav(state, &user(headers))?,
        stored,
        &login,
        state.secrets.available(),
        notice,
    ))
}

/// The `last_refresh` time of a stored `auth.json` as a `YYYY-MM-DD HH:MM:SS`
/// string for the dashboard time helper, or `None` when the field is absent or
/// not an RFC 3339 time. The time carries no secret.
fn codex_last_refresh(auth: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(auth).ok()?;
    let text = value.get("last_refresh")?.as_str()?;
    let date = text.get(0..10)?;
    let time = text.get(11..19)?;
    (text.as_bytes().get(10) == Some(&b'T') && time.as_bytes().get(2) == Some(&b':'))
        .then(|| format!("{date} {time}"))
}

fn mcp_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    Ok(pages::settings_mcp(
        &user(headers),
        &settings_nav(state, &user(headers))?,
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
        .store
        .projects_for(&user(headers))?
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
        &settings_nav(state, &user(headers))?,
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
    problem: Option<&str>,
) -> Result<Markup, AppError> {
    project_page_with(state, headers, name, selected, notice, problem, None)
}

fn project_page_with(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    selected: pages::PipelinePart,
    notice: Option<&str>,
    problem: Option<&str>,
    maintenance: Option<Maintenance>,
) -> Result<Markup, AppError> {
    let project = visible_project(state, headers, name)?;
    let mut settings = project_settings(state, name, &project)?;
    if let Some(maintenance) = maintenance {
        settings.maintenance = maintenance;
    }
    Ok(pages::settings_project(
        &user(headers),
        &settings_nav(state, &user(headers))?,
        &settings,
        selected,
        notice,
        problem,
    ))
}

/// The `?stage=` value of a project's settings page: the stage, gate or merge
/// whose panel is open.
#[derive(Deserialize)]
struct StageQuery {
    stage: Option<String>,
    notice: Option<String>,
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
    let agents = state
        .registry
        .entries()
        .map(|entry| pages::AgentChoice {
            id: entry.id().to_owned(),
            display_name: entry.display_name().to_owned(),
            available: entry.available(),
            efforts: entry
                .efforts()
                .iter()
                .map(|&level| level.to_owned())
                .collect(),
            default_effort: entry.default_effort().to_owned(),
        })
        .collect();
    Ok(ProjectSettings {
        name: name.to_owned(),
        models: state.store.project_models(name, &project.models)?,
        efforts: state.store.project_efforts(name)?,
        agents,
        agent_selection: state.store.project_agents(name)?,
        auto_merge: state.store.project_auto_merge(name, project.auto_merge)?,
        llm_review: state.store.project_llm_review(name, project.llm_review)?,
        gates: state.store.project_gates(name)?,
        default_gates: state.store.settings()?.gates,
        servers,
        collaborators: state.store.collaborators(name)?,
        maintenance: state.store.project_maintenance(name)?,
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

async fn show_renovate(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    renovate_page(&state, &headers, None, None)
}

async fn show_claude(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    claude_page(&state, &headers, None)
}

/// Sends the old Claude account URL to the agent's page under Agents, so a
/// bookmark or a link made before the move still lands on the token card.
async fn redirect_claude() -> Redirect {
    Redirect::to("/settings/agents/claude")
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
    project_page(
        &state,
        &headers,
        &name,
        query.part(),
        query.notice.as_deref(),
        None,
    )
}

fn default_effort() -> String {
    factory_core::DEFAULT_AGENT.default_effort().to_owned()
}

fn default_agent() -> String {
    factory_core::DEFAULT_AGENT.id().to_owned()
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

    /// An agent field, or the default agent when the field is absent, so a form
    /// posted before the selector existed still saves.
    fn agent(&self, key: &str) -> String {
        self.get(key).map_or_else(default_agent, str::to_owned)
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
    visible_project(&state, &headers, &name)?;
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
    let agents = StageAgents {
        clarify: form.agent("clarify_agent"),
        spec: form.agent("spec_agent"),
        plan: form.agent("plan_agent"),
        implement: form.agent("implement_agent"),
        review: form.agent("review_agent"),
    };

    // Validate the agents and the efforts before any write, so a rejected agent
    // or effort stores none of the agents, efforts or models. Each effort is
    // checked against the agent its stage is being saved with.
    Store::check_agents(&agents)?;
    Store::check_efforts(&efforts, &agents)?;
    state.store.save_project_efforts(&name, &efforts, &agents)?;
    state.store.save_project_agents(&name, &agents)?;
    state.store.save_project_models(&name, &models)?;
    state.store.save_project_settings(
        &name,
        form.ticked("auto_merge"),
        form.ticked("llm_review"),
    )?;
    save_posted_gates(&state.store, &name, &form)?;
    save_pipeline_mcp(&state, &name, &form)?;
    state.wake.notify_one();
    project_page(&state, &headers, &name, query.part(), Some("Saved."), None)
}

#[derive(Deserialize)]
struct CollaboratorForm {
    #[serde(default)]
    login: String,
}

async fn add_project_collaborator(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<CollaboratorForm>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &project)?;
    let part = pages::PipelinePart::parse(None);
    let login = form.login.trim();
    if login.is_empty() {
        let problem = "Type a Tailscale login to add a collaborator.";
        return project_page(&state, &headers, &project, part, None, Some(problem));
    }
    if state.store.is_collaborator(&project, login)? {
        let problem = format!("{login} is already a collaborator.");
        return project_page(&state, &headers, &project, part, None, Some(&problem));
    }
    state
        .store
        .add_collaborator(&project, login, &user(&headers))?;
    project_page(
        &state,
        &headers,
        &project,
        part,
        Some(&format!("Added {login}.")),
        None,
    )
}

async fn remove_project_collaborator(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<CollaboratorForm>,
) -> Result<Response, AppError> {
    visible_project(&state, &headers, &project)?;
    let part = pages::PipelinePart::parse(None);
    let login = form.login.trim();
    match state.store.remove_collaborator(&project, login) {
        Ok(()) => {}
        Err(StoreError::LastCollaborator) => {
            let problem = StoreError::LastCollaborator.to_string();
            return Ok(
                project_page(&state, &headers, &project, part, None, Some(&problem))?
                    .into_response(),
            );
        }
        Err(err) => return Err(err.into()),
    }
    if login == user(&headers) {
        return Ok(Redirect::to("/settings").into_response());
    }
    Ok(project_page(
        &state,
        &headers,
        &project,
        part,
        Some(&format!("Removed {login}.")),
        None,
    )?
    .into_response())
}

#[derive(Deserialize)]
struct MaintenanceForm {
    renovate: Option<String>,
    packaging: Option<String>,
    #[serde(default)]
    packaging_type: String,
    #[serde(default)]
    manifest_files: String,
    #[serde(default)]
    build_config_files: String,
    #[serde(default)]
    upstream_repo: String,
}

fn path_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn save_project_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<MaintenanceForm>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &project)?;
    let part = pages::PipelinePart::parse(None);
    let kind = if form.packaging_type.is_empty() {
        Some(PackagingType::default())
    } else {
        PackagingType::parse(&form.packaging_type)
    };
    let packaging = form.packaging.is_some().then(|| Packaging {
        kind: kind.unwrap_or_default(),
        manifest_files: path_lines(&form.manifest_files),
        build_config_files: path_lines(&form.build_config_files),
        upstream_repo: form.upstream_repo.trim().to_owned(),
    });
    let maintenance = Maintenance {
        renovate: form.renovate.is_some(),
        packaging,
    };
    let refusal = match (&maintenance.packaging, kind) {
        (Some(_), None) => Some("The packaging type must be snap, deb or other.".to_owned()),
        _ => maintenance.check().err(),
    };
    if let Some(problem) = refusal {
        return project_page_with(
            &state,
            &headers,
            &project,
            part,
            None,
            Some(&problem),
            Some(maintenance),
        );
    }
    state
        .store
        .save_project_maintenance(&project, &maintenance)?;
    project_page(&state, &headers, &project, part, Some("Saved."), None)
}

#[derive(Deserialize)]
struct RemoveProjectForm {
    #[serde(default)]
    confirm: String,
}

async fn remove_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<RemoveProjectForm>,
) -> Result<Response, AppError> {
    visible_project(&state, &headers, &project)?;
    if form.confirm != project {
        let problem = format!("Nothing was removed. Type {project} to remove the project.");
        return Ok(project_page(
            &state,
            &headers,
            &project,
            pages::PipelinePart::parse(None),
            None,
            Some(&problem),
        )?
        .into_response());
    }
    state.store.remove_project(&project)?;
    state.wake.notify_one();
    let notice = format!("Removed project {project}.");
    Ok(Redirect::to(&format!("/settings?notice={}", query_escape(&notice))).into_response())
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

/// The project and server named in a project's MCP route, or a 404. A project
/// the login may not see reads as unknown, so its MCP pages 404 like any other.
fn project_and_server(
    state: &AppState,
    headers: &HeaderMap,
    project: &str,
    server: &str,
) -> Result<McpServer, AppError> {
    visible_project(state, headers, project)?;
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
    let registered = project_and_server(state, headers, project, server)?;
    Ok(pages::settings_project_mcp(
        &user(headers),
        &settings_nav(state, &user(headers))?,
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
    let registered = project_and_server(&state, &headers, &project, &server)?;
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
        let total = deletes.len();
        let visible = state.store.projects_for(&user(&headers))?;
        let shown = deletes
            .into_iter()
            .filter(|(project, _)| visible.iter().any(|name| name == project))
            .collect();
        let pending = pages::PendingEdit {
            url: url.to_owned(),
            headers: form.headers,
            deletes: shown,
            total,
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
    headers: HeaderMap,
    Path((project, server)): Path<(String, String)>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &project)?;
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

#[derive(Deserialize)]
struct RenovateForm {
    #[serde(default)]
    auto_reruns: String,
    #[serde(default)]
    total_reruns: String,
}

fn parse_renovate_limits(form: &RenovateForm) -> Result<(u32, u32), String> {
    let whole = |value: &str, name: &str| {
        value
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("{name} must be a whole number"))
    };
    let auto = whole(&form.auto_reruns, "Automatic reruns")?;
    let total = whole(&form.total_reruns, "Total reruns")?;
    if total < 1 {
        return Err("Total reruns must be at least 1".into());
    }
    if auto > total {
        return Err("Automatic reruns must not be more than total reruns".into());
    }
    Ok((auto, total))
}

async fn save_renovate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RenovateForm>,
) -> Result<Markup, AppError> {
    let (auto, total) = match parse_renovate_limits(&form) {
        Ok(limits) => limits,
        Err(problem) => return renovate_page(&state, &headers, None, Some(&problem)),
    };
    let settings = Settings {
        renovate_auto_reruns: auto,
        renovate_total_reruns: total,
        ..state.store.settings()?
    };
    state.store.save_settings(&settings)?;
    state.wake.notify_one();
    renovate_page(&state, &headers, Some("Saved."), None)
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
/// afresh, so the next worker run uses the new one with no restart.
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

async fn show_codex(State(state): State<AppState>, headers: HeaderMap) -> Result<Markup, AppError> {
    codex_page(&state, &headers, None).await
}

#[derive(Deserialize)]
struct CodexAuthForm {
    #[serde(default)]
    auth: String,
}

/// Replaces the stored Codex login with the pasted `auth.json`. The value is
/// trimmed of surrounding whitespace. An empty field keeps the stored login, so
/// a blank save is a no-op. A paste that is not a subscription `auth.json` is
/// refused with a message that names the missing field and never echoes the
/// input, and stores nothing. The next Codex run reads the login afresh, so a
/// save applies with no restart.
async fn save_codex_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CodexAuthForm>,
) -> Result<Markup, AppError> {
    let auth = form.auth.trim();
    if auth.is_empty() {
        return codex_page(&state, &headers, Some("Saved.")).await;
    }
    match validate_codex_auth(auth) {
        Ok(()) => {
            state.secrets.set_codex_auth(auth)?;
            codex_page(&state, &headers, Some("Saved.")).await
        }
        Err(err) => codex_page(&state, &headers, Some(&err.to_string())).await,
    }
}

/// Clears the stored Codex login. Clearing works even when the master key is
/// missing, so a login stranded by a lost key can still be removed.
async fn clear_codex_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    state.secrets.clear_codex_auth()?;
    codex_page(&state, &headers, Some("Cleared.")).await
}

/// Starts a device login, or leaves a running one alone, then redraws the page
/// with the login area polling for the URL and code.
async fn start_codex_login(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    let image = state.store.settings()?.worker_image;
    state.device_login.start(image).await;
    codex_page(&state, &headers, None).await
}

/// The login area the page polls for the URL, the code, success or a failure
/// reason.
async fn codex_login_fragment(State(state): State<AppState>) -> Markup {
    let login = state.device_login.snapshot().await;
    pages::codex_login_fragment(&login, state.secrets.available())
}

/// Cancels a running device login, which removes its container, then redraws
/// the page.
async fn cancel_codex_login(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    state.device_login.cancel().await;
    codex_page(&state, &headers, None).await
}

async fn show_copilot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    copilot_page(&state, &headers, None).await
}

#[derive(Deserialize)]
struct CopilotTokenForm {
    #[serde(default)]
    token: String,
}

async fn save_copilot_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CopilotTokenForm>,
) -> Result<Markup, AppError> {
    if form.token.trim().is_empty() {
        return copilot_page(&state, &headers, Some("Saved.")).await;
    }
    match validate_copilot_token(&form.token) {
        Ok(token) => {
            state.secrets.set_copilot_token(token)?;
            copilot_page(&state, &headers, Some("Saved.")).await
        }
        Err(err) => copilot_page(&state, &headers, Some(&err.to_string())).await,
    }
}

async fn clear_copilot_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    state.secrets.clear_copilot_token()?;
    copilot_page(&state, &headers, Some("Cleared.")).await
}

async fn start_copilot_login(State(state): State<AppState>) -> Result<Redirect, AppError> {
    let image = state.store.settings()?.worker_image;
    state.copilot_login.start(image).await;
    Ok(Redirect::to("/settings/agents/copilot"))
}

async fn copilot_login_fragment(State(state): State<AppState>) -> Response {
    let login = state.copilot_login.snapshot().await;
    let fragment = pages::copilot_login_fragment(&login, state.secrets.available());
    if login == crate::device_login::LoginView::SignedIn {
        return ([("HX-Refresh", "true")], fragment).into_response();
    }
    fragment.into_response()
}

async fn cancel_copilot_login(State(state): State<AppState>) -> Redirect {
    state.copilot_login.cancel().await;
    Redirect::to("/settings/agents/copilot")
}

#[derive(Deserialize)]
struct CodexPriceForm {
    #[serde(default)]
    model: String,
    #[serde(default)]
    input: String,
    #[serde(default)]
    cached_input: String,
    #[serde(default)]
    output: String,
}

/// Parses a USD-per-million-tokens price field: a finite, non-negative number.
/// Anything else, such as an empty field or text, is refused with a message that
/// names the field.
fn parse_price(field: &str, value: &str) -> Result<f64, String> {
    match value.trim().parse::<f64>() {
        Ok(price) if price.is_finite() && price >= 0.0 => Ok(price),
        _ => Err(format!(
            "The {field} price must be a number that is zero or more."
        )),
    }
}

/// Adds a Codex model price or replaces the row for its model, so the same form
/// adds a new model and edits an existing one. A blank model or a price that is
/// not a non-negative number is refused and stores nothing. The next Codex run
/// reads the price afresh, so a change applies with no restart.
async fn save_codex_price(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CodexPriceForm>,
) -> Result<Markup, AppError> {
    let model = form.model.trim();
    if model.is_empty() {
        return codex_page(&state, &headers, Some("The model must not be empty.")).await;
    }
    let price = match (
        parse_price("input", &form.input),
        parse_price("cached input", &form.cached_input),
        parse_price("output", &form.output),
    ) {
        (Ok(input), Ok(cached_input), Ok(output)) => crate::store::CodexPrice {
            model: model.to_owned(),
            input_usd_per_mtok: input,
            cached_input_usd_per_mtok: cached_input,
            output_usd_per_mtok: output,
        },
        (input, cached_input, output) => {
            let reason = input.err().or(cached_input.err()).or(output.err());
            return codex_page(&state, &headers, reason.as_deref()).await;
        }
    };
    state.store.set_codex_price(&price)?;
    codex_page(&state, &headers, Some("Saved.")).await
}

/// Deletes the Codex price for the posted model. A model with no row is a no-op.
async fn delete_codex_price(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CodexPriceForm>,
) -> Result<Markup, AppError> {
    state.store.delete_codex_price(form.model.trim())?;
    codex_page(&state, &headers, Some("Deleted.")).await
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

async fn show_onboard(headers: HeaderMap, Query(query): Query<NoticeQuery>) -> Markup {
    let view = pages::OnboardView {
        name: "",
        repo: "",
        base: DEFAULT_BASE,
        checks: "",
        models: &StageModels::default(),
        auto_merge: false,
        llm_review: false,
    };
    pages::onboard(&user(&headers), &view, None, query.notice.as_deref())
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

    let _one_onboarding_at_a_time = state.onboarding.lock().await;
    let mut report = onboard_report(&state, effective_name, repo, base).await?;

    if matches!(form.action.as_str(), "create" | "skip") && report.errors.is_empty() {
        let _pass = state.slots.lock_pass().await;
        let leftover = state.store.job_ids_for(effective_name)?;
        if leftover.iter().any(|id| state.slots.has_task(*id)) {
            report.errors.push(
                "This name's removed project still has a job finishing. Try again in a minute."
                    .to_owned(),
            );
        } else {
            for id in &leftover {
                state.store.delete_job(*id, &state.data_dir)?;
            }
        }
    }

    if matches!(form.action.as_str(), "create" | "skip") && report.errors.is_empty() {
        let checks = (!checks.is_empty()).then_some(checks);
        match state
            .store
            .create_project(effective_name, repo, base, checks, Some(&user(&headers)))
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
    Ok(pages::onboard(&user(&headers), &view, Some(&report), None).into_response())
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

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
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
