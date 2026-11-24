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
use factory_core::{Agent, Command, DEFAULT_AGENT, Event, Gates, Stage, Wait};
use factory_runner::{Access, Account, Expect, ForgeAccount, ForgeKind};
use maud::{Markup, html};
use serde::Deserialize;
use tokio::sync::Notify;

use crate::accounts::{Accounts, ResolveError, TokenRow, UpstreamCredential};
use crate::check::{CheckOutcome, McpChecker, ToolList};
use crate::copilot::validate_copilot_token;
use crate::csrf::CsrfKey;
use crate::device_login::DeviceLogin;
use crate::forge_tokens::{self, SaveError};
use crate::forgejo_instances::{ForgejoProbe, HttpForgejoProbe};
use crate::forges::Forges;
use crate::github::RepoReport;
use crate::mcp::{McpError, McpProjectSettings, McpServer, McpStages, McpTransport};
use crate::pages::{self, HeaderState, JobLinks, ProjectSettings, ServerSettings, Workers};
use crate::projects::{
    Project, ProjectForge, ProjectLabel, Projects, StageAgents, StageEfforts, StageModels,
};
use crate::proxy::{StoreUpstreams, Upstream, Upstreams};
use crate::registry::AgentRegistry;
use crate::secrets::{ForgeTokenOwner, SecretError, SecretRef, SecretStore, validate_codex_auth};
use crate::settings::Settings;
use crate::store::{
    FACTORY_REQUESTER, JobFile, JobRow, Maintenance, MaintenanceSave, Packaging, PackagingType,
    PrerequisiteEdit, PrerequisiteView, ProjectGuard, ProjectTotals, Store, StoreError,
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
    forges: Forges,
    slots: Arc<WorkerSlots>,
    secrets: Arc<dyn SecretStore>,
    checker: Arc<dyn McpChecker>,
    forgejo_probe: Arc<dyn ForgejoProbe>,
    registry: Arc<AgentRegistry>,
    device_login: Arc<DeviceLogin>,
    onboarding: Arc<tokio::sync::Mutex<()>>,
    copilot_login: Arc<DeviceLogin>,
    admins: Arc<Vec<String>>,
    csrf: Arc<CsrfKey>,
}

impl AppState {
    fn known_forge(&self, name: &str) -> Result<Option<ForgeKind>, StoreError> {
        self.store.resolve_forge(name)
    }

    fn forges(&self) -> Result<Vec<ForgeKind>, StoreError> {
        self.store.forge_kinds()
    }
}

/// The dashboard. Every change it makes to a job notifies `wake`, so the
/// job loop looks at the job right away. Uploaded images go under
/// `<data_dir>/jobs/<id>/images/`. The index page reads `plan_usage` from the
/// cache the worker runs fill.
/// The onboarding wizard runs its repository checks through `forges`. The
/// board shows the worker slots the scheduler fills in `slots`. Per-project
/// MCP header values are read and written through `secrets`, which reports
/// itself unavailable when no master key is present. The per-project MCP
/// connection check runs through `checker`, the way the onboarding checks run
/// through `forges`. The settings pages list the agents and link their account
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
    forges: Forges,
    slots: Arc<WorkerSlots>,
    secrets: Arc<dyn SecretStore>,
    checker: Arc<dyn McpChecker>,
    registry: Arc<AgentRegistry>,
    device_login: Arc<DeviceLogin>,
    copilot_login: Arc<DeviceLogin>,
    admins: Vec<String>,
) -> Router {
    app_with_forgejo_probe(
        store,
        projects,
        wake,
        data_dir,
        plan_usage,
        forges,
        slots,
        secrets,
        checker,
        Arc::new(HttpForgejoProbe::new(&[])),
        registry,
        device_login,
        copilot_login,
        admins,
    )
}

/// As [`app`], reading the version of a Forgejo server through `forgejo_probe`
/// when an admin registers one.
#[allow(
    clippy::too_many_arguments,
    reason = "the dashboard wires each long-lived dependency the handlers share"
)]
pub fn app_with_forgejo_probe(
    store: Arc<Store>,
    projects: Arc<Projects>,
    wake: Arc<Notify>,
    data_dir: PathBuf,
    plan_usage: Arc<PlanUsageCache>,
    forges: Forges,
    slots: Arc<WorkerSlots>,
    secrets: Arc<dyn SecretStore>,
    checker: Arc<dyn McpChecker>,
    forgejo_probe: Arc<dyn ForgejoProbe>,
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
        forges,
        slots,
        secrets,
        checker,
        forgejo_probe,
        registry,
        device_login,
        onboarding: Arc::new(tokio::sync::Mutex::new(())),
        copilot_login,
        admins: Arc::new(admins),
        csrf: Arc::new(CsrfKey::generate()),
    };
    Router::new()
        .route("/", get(index))
        .route("/notifications", get(notifications))
        .route("/fragments/plan-usage", get(plan_usage_fragment))
        .route("/fragments/stats", get(stats_fragment))
        .route("/fragments/workers", get(workers_fragment))
        .route("/fragments/board", get(board_fragment))
        .route("/fragments/picker", get(picker_fragment))
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
        .route("/jobs/{id}/fix", post(start_fix))
        .route("/jobs/{id}/dismiss", post(dismiss))
        .route("/jobs/{id}/changes", post(request_changes))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/settings", get(settings_home))
        .route("/settings/forges/{forge}", get(show_forge))
        .route("/settings/forges/{forge}/token", post(save_forge_token))
        .route(
            "/settings/forges/{forge}/token/clear",
            post(clear_forge_token),
        )
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
            "/settings/projects/{project}/forge-token",
            post(save_project_forge_token),
        )
        .route(
            "/settings/projects/{project}/forge-token/clear",
            post(clear_project_forge_token),
        )
        .route(
            "/settings/projects/{project}/use-my-account",
            post(set_use_my_account),
        )
        .merge(project_card_routes())
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
        .route(
            "/settings/github",
            get(redirect_polling_get).post(redirect_polling_post),
        )
        .route("/settings/polling", get(show_polling).post(save_polling))
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
        .route(
            "/settings/forgejo",
            get(show_forgejo).post(add_forgejo_instance),
        )
        .route(
            "/settings/forgejo/{name}/delete",
            post(delete_forgejo_instance),
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

fn project_card_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/settings/projects/{project}/maintenance",
            post(save_project_maintenance),
        )
        .route(
            "/settings/projects/{project}/checks",
            post(save_project_checks),
        )
        .route(
            "/settings/projects/{project}/packages",
            post(save_project_packages),
        )
        .route("/settings/projects/{project}/name", post(save_project_name))
        .route(
            "/settings/projects/{project}/maintenance/check",
            post(check_project_upstream),
        )
        .route(
            "/settings/projects/{project}/upstream-token",
            post(save_upstream_token),
        )
        .route(
            "/settings/projects/{project}/upstream-token/clear",
            post(clear_upstream_token_route),
        )
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
    Forbidden(String),
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
            AppError::Forbidden(message) => (StatusCode::FORBIDDEN, message),
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
            AppError::Store(
                err @ (StoreError::InvalidEffort(_)
                | StoreError::InvalidAgent(_)
                | StoreError::InvalidPrerequisite(_)),
            ) => (StatusCode::BAD_REQUEST, err.to_string()),
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
    names: Vec<ProjectLabel>,
    attention: Vec<pages::ProjectAttention>,
    selected: Option<String>,
    jobs: Vec<JobRow>,
    costs: HashMap<i64, f64>,
    waits: HashMap<i64, Wait>,
    totals: ProjectTotals,
    auto_merge: bool,
    project: Option<Project>,
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

/// The projects `login` collaborates on, as pages label them, in shown-name order.
fn project_labels(state: &AppState, login: &str) -> Result<Vec<ProjectLabel>, StoreError> {
    state.store.project_labels_for(login)
}

fn project_label(state: &AppState, name: &str) -> Result<ProjectLabel, StoreError> {
    Ok(ProjectLabel {
        id: name.to_owned(),
        shown: state.store.project_shown_name(name)?,
    })
}

fn load_board(
    state: &AppState,
    login: &str,
    project: Option<String>,
    cookie_project: Option<String>,
) -> Result<BoardData, AppError> {
    let names = project_labels(state, login)?;
    let selected = match project {
        Some(name) if !names.iter().any(|known| known.id == name) => {
            return Err(AppError::UnknownProject(name));
        }
        Some(name) => Some(name),
        None => cookie_project
            .filter(|name| names.iter().any(|known| &known.id == name))
            .or_else(|| names.first().map(|label| label.id.clone())),
    };
    let jobs = state.store.list()?;
    let costs = state.store.costs()?;
    let waits = state.store.waits()?;
    let defaults: HashMap<String, bool> = state
        .projects
        .list()?
        .into_iter()
        .map(|(name, project)| (name, project.auto_merge))
        .collect();
    let auto_merges: HashMap<String, bool> = state
        .store
        .project_auto_merges(login)?
        .into_iter()
        .map(|(name, saved)| {
            let default = defaults.get(&name).copied().unwrap_or(false);
            (name, saved.unwrap_or(default))
        })
        .collect();
    let attention = pages::project_attention(&names, &jobs, &waits, &auto_merges);
    let (totals, auto_merge, project) = match &selected {
        Some(name) => (
            state.store.project_totals(name)?,
            auto_merges.get(name).copied().unwrap_or(false),
            state.projects.lookup(name)?,
        ),
        None => (ProjectTotals::default(), false, None),
    };
    Ok(BoardData {
        names,
        attention,
        selected,
        jobs,
        costs,
        waits,
        totals,
        auto_merge,
        project,
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
) -> Result<Response, AppError> {
    let login = user(&headers);
    let explicit = query.project.is_some();
    let data = load_board(&state, &login, query.project, cookie_project(&headers))?;
    let plan_usage = usage_tile(&state);
    let notifications = state.store.notifications_enabled(&login)?;
    let project_token_missing = match (
        data.selected.as_deref(),
        data.project.as_ref().and_then(|p| p.forge.kind()),
    ) {
        (Some(project), Some(forge)) => state
            .store
            .forge_account(ForgeTokenOwner::Project { project, forge })?
            .is_none(),
        _ => false,
    };
    let page = pages::index(
        &login,
        &data.names,
        &data.attention,
        data.selected.as_deref(),
        &data.jobs,
        &data.costs,
        &data.waits,
        data.totals,
        plan_usage.as_ref(),
        data.auto_merge,
        data.project.as_ref(),
        &data.workers,
        notifications,
        project_token_missing,
    );
    match data.selected.as_deref().filter(|_| explicit) {
        Some(name) => {
            let cookie = format!(
                "{PROJECT_COOKIE}={}; Path=/; SameSite=Lax; HttpOnly; Max-Age=31536000",
                query_escape(name)
            );
            Ok(([(header::SET_COOKIE, cookie)], page).into_response())
        }
        None => Ok(page.into_response()),
    }
}

const PROJECT_COOKIE: &str = "factory_project";

fn cookie_project(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(PROJECT_COOKIE)?.strip_prefix('='))
        .and_then(query_unescape)
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
            let kind = pages::attention(
                &row.job,
                merge,
                snapshot.waits.get(&row.id).unwrap_or(&Wait::Ready),
            )?;
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
    let names = project_labels(&state, &user(&headers))?;
    if let Some(project) = &query.project
        && !names.iter().any(|known| &known.id == project)
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
    let data = load_board(&state, &user(&headers), query.project, None)?;
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
    let data = load_board(&state, &user(&headers), query.project, None)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {}.into_response());
    };
    Ok(region_response(
        version.as_deref(),
        pages::workers(&data.workers, &data.jobs, &data.names, project),
    ))
}

async fn picker_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let version = query.v.clone();
    let data = load_board(&state, &user(&headers), query.project.clone(), None)?;
    let selected = if query.project.is_some() {
        data.selected.as_deref().unwrap_or_default()
    } else {
        ""
    };
    Ok(region_response(
        version.as_deref(),
        pages::picker(&data.attention, selected, true),
    ))
}

async fn board_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let version = query.v.clone();
    let data = load_board(&state, &user(&headers), query.project, None)?;
    let Some(project) = data.selected.as_deref() else {
        return Ok(html! {}.into_response());
    };
    Ok(region_response(
        version.as_deref(),
        pages::board(
            &data.rows(),
            &data.costs,
            &data.waits,
            data.auto_merge,
            project,
            data.project.as_ref(),
        ),
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

/// Reads a posted job number, such as a prerequisite of a request.
async fn read_job_number(field: Field<'_>) -> Result<i64, AppError> {
    let value = read_text(field).await?;
    value
        .trim()
        .parse::<i64>()
        .map_err(|_| AppError::BadRequest(format!("{value:?} is not a job number to wait for")))
}

async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    let mut project = None;
    let mut request = None;
    let mut after = Vec::new();
    let mut attachments = Vec::new();
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|err| multipart_error(&err))?
    {
        match field.name() {
            Some("project") => project = Some(read_text(field).await?),
            Some("request") => request = Some(read_text(field).await?),
            Some("after") => after.push(read_job_number(field).await?),
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
    let id = state
        .store
        .create_after(&project, &user(&headers), request, &after)?;
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
    links: JobLinks,
    prerequisites: Vec<PrerequisiteView>,
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
        Expect::Review | Expect::FailureAnalysis | Expect::DiffCheck | Expect::PackagingGaps => {
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

fn load_job(state: &AppState, login: &str, id: i64) -> Result<JobData, AppError> {
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
    let links = job_links(state, &row, login)?;
    let prerequisites = state.store.prerequisites(id)?;
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
        links,
        prerequisites,
    })
}

fn job_links(state: &AppState, row: &JobRow, login: &str) -> Result<JobLinks, StoreError> {
    let store = &state.store;
    let mut links = JobLinks {
        csrf: state.csrf.issue(login),
        ..JobLinks::default()
    };
    if let Some(pr) = row.job.renovate_pr() {
        for &gap in pr.gaps.iter().flatten() {
            match store.get(gap) {
                Ok(found) if found.job.gap_source() == Some((row.id, pr.number)) => {
                    links.gaps.push(gap);
                }
                Ok(_) | Err(StoreError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        if let Some(fix) = &pr.fix {
            links.fix = match store.get(fix.job) {
                Ok(found)
                    if found.project == row.project
                        && found.job.fix_source() == Some((row.id, pr.number)) =>
                {
                    Some(fix.job)
                }
                Ok(_) | Err(StoreError::NotFound(_)) => None,
                Err(err) => return Err(err),
            };
        }
    }
    if let Some((source, number)) = row.job.fix_source() {
        links.source = match store.get(source) {
            Ok(found)
                if found.project == row.project
                    && found
                        .job
                        .renovate_pr()
                        .is_some_and(|pr| pr.number == number) =>
            {
                Some(source)
            }
            Ok(_) | Err(StoreError::NotFound(_)) => None,
            Err(err) => return Err(err),
        };
    }
    Ok(links)
}

async fn show_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Markup, AppError> {
    visible_job(&state, &headers, id)?;
    let data = load_job(&state, &user(&headers), id)?;
    let images = state.store.images(id)?;
    let files = state.store.files(id)?;
    let label = project_label(&state, &data.row.project)?;
    Ok(pages::job(
        &user(&headers),
        &data.row,
        &label,
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
        &data.links,
        &data.prerequisites,
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
    let data = load_job(&state, &user(&headers), id)?;
    let label = project_label(&state, &data.row.project)?;
    Ok(pages::job_fragment(
        &data.row,
        &label,
        data.project.as_ref(),
        &data.settings,
        &data.logs,
        &data.events,
        data.cost_usd,
        &data.models,
        &data.efforts,
        data.auto_merge,
        &data.links,
        &data.prerequisites,
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
    let prerequisites = state.store.prerequisites(id)?;
    let candidates = state.store.prerequisite_candidates(id)?;
    Ok(pages::edit_request(
        &user(&headers),
        id,
        &row.job,
        &images,
        &files,
        &prerequisites,
        &candidates,
    )
    .into_response())
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
    let mut after = Vec::new();
    let mut keep = Vec::new();
    let mut attachments = Vec::new();
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|err| multipart_error(&err))?
    {
        match field.name() {
            Some("request") => request = Some(read_text(field).await?),
            Some("remove") => removes.push(read_text(field).await?),
            Some("after") => after.push(read_job_number(field).await?),
            Some("keep") => keep.push(read_job_number(field).await?),
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
    let combined = kept_then_new(&job_dir, &kept_images, &kept_files, attachments)?;

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
        &PrerequisiteEdit {
            keep: &keep,
            after: &after,
        },
    );
    let _ = std::fs::remove_dir_all(&staging_dir);
    result?;
    state.wake.notify_one();
    Ok(job_redirect(id))
}

fn kept_then_new(
    job_dir: &std::path::Path,
    kept_images: &[&String],
    kept_files: &[&JobFile],
    new: Vec<Attachment>,
) -> Result<Vec<Attachment>, AppError> {
    let mut combined = Vec::new();
    for name in kept_images {
        let bytes = read_stored(&job_dir.join("images").join(name))?;
        combined.push(Attachment::Image {
            extension: image_ext(name)?,
            bytes,
        });
    }
    for file in kept_files {
        let bytes = read_stored(&job_dir.join("files").join(&file.name))?;
        combined.push(Attachment::File {
            extension: file_ext(&file.name)?,
            name: Some(file.original_name.clone()),
            bytes,
        });
    }
    combined.extend(new);
    Ok(combined)
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
    let row = visible_job(&state, &headers, id)?;
    let redirect = if *row.job.stage() == Stage::Proposed {
        state.store.approve_proposal(id, &user(&headers))?;
        state.wake.notify_one();
        job_redirect(id)
    } else {
        apply(&state, id, Event::Approve)?
    };
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

#[derive(Deserialize)]
struct FixForm {
    project: Option<String>,
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    instructions: String,
}

async fn start_fix(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<FixForm>,
) -> Result<Redirect, AppError> {
    visible_job(&state, &headers, id)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    let instructions = form.instructions.trim();
    let fix = state.store.start_fix(
        id,
        &user(&headers),
        (!instructions.is_empty()).then_some(instructions),
    )?;
    state.wake.notify_one();
    state
        .store
        .append_factory_log(id, &format!("Started fix job #{fix} from the dashboard."))?;
    board_redirect(&state, form.project, job_redirect(id))
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

fn query_unescape(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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
    let names = project_labels(&state, &user(&headers))?;
    let tail = match &query.notice {
        Some(notice) => format!("?notice={}", query_escape(notice)),
        None => String::new(),
    };
    Ok(match names.first() {
        Some(label) => Redirect::to(&format!(
            "/settings/projects/{}{tail}",
            query_escape(&label.id)
        )),
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
        projects: project_labels(state, login)?,
        forges: state.forges()?,
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

fn polling_page(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    Ok(pages::settings_polling(
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
    let usage = project_labels(state, &user(headers))?
        .into_iter()
        .map(|project| {
            let settings = server_settings(state, &project.id, server.clone())?;
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
    project_page_full(
        state,
        headers,
        name,
        selected,
        notice,
        problem,
        maintenance,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the page shows a notice, a problem, unsaved maintenance fields and a check result at once"
)]
fn project_page_full(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    selected: pages::PipelinePart,
    notice: Option<&str>,
    problem: Option<&str>,
    maintenance: Option<Maintenance>,
    check: Option<pages::UpstreamCheckView>,
) -> Result<Markup, AppError> {
    let project = visible_project(state, headers, name)?;
    let mut settings = project_settings(state, &user(headers), name, &project)?;
    if let Some(maintenance) = maintenance {
        settings.maintenance = maintenance;
    }
    settings.upstream.check = check;
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
    login: &str,
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
            models: entry
                .model_suggestions()
                .iter()
                .map(|&model| model.to_owned())
                .collect(),
        })
        .collect();
    let (maintenance, unknown_upstream) = state.store.project_maintenance_lenient(name)?;
    let forge_account = match project.forge.kind() {
        Some(kind) => Some(pages::ForgeAccountCard {
            kind: kind.clone(),
            repo: project.repo.clone(),
            project_account: state.store.forge_account(ForgeTokenOwner::Project {
                project: name,
                forge: kind,
            })?,
            secrets_available: state.secrets.available(),
            csrf: state.csrf.issue(login),
            use_my_account: state.store.use_my_account(name, login)?,
            has_user_token: state
                .store
                .forge_account(ForgeTokenOwner::User { login, forge: kind })?
                .is_some(),
        }),
        None => None,
    };
    Ok(ProjectSettings {
        name: name.to_owned(),
        shown: project_label(state, name)?.shown,
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
        maintenance,
        forge_problem: project.forge.problem(),
        upstream_problem: unknown_upstream.map(|name| {
            format!(
                "The upstream forge {name:?} is no longer registered, so the upstream is shown as GitHub. Pick the upstream forge and save, or a Renovate job that needs the upstream fails."
            )
        }),
        forge_account,
        upstream: pages::UpstreamCard {
            accounts: state
                .forges()?
                .into_iter()
                .map(|forge| {
                    let owner = ForgeTokenOwner::Upstream {
                        project: name,
                        forge: &forge,
                    };
                    let account = state.store.forge_account(owner)?;
                    Ok((forge, account))
                })
                .collect::<Result<_, StoreError>>()?,
            secrets_available: state.secrets.available(),
            check: None,
        },
        checks: project.checks.clone(),
        packages: state.store.project_packages(name)?,
        checks_csrf: state.csrf.issue(login),
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

async fn show_polling(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    polling_page(&state, &headers, None)
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

/// Sends the old GitHub settings URL to the polling page.
async fn redirect_polling_get() -> Redirect {
    Redirect::to("/settings/polling")
}

/// A 307 keeps the method and body, so a form opened before the move still
/// saves.
async fn redirect_polling_post() -> Redirect {
    Redirect::temporary("/settings/polling")
}

/// Sends the old Claude account URL to the agent's page under Agents, so a
/// bookmark or a link made before the move still lands on the token card.
async fn redirect_claude() -> Redirect {
    Redirect::to("/settings/agents/claude")
}

/// What the Forgejo page shows beyond the registry: the form's previous
/// input and a problem or notice from the last submit.
#[derive(Default)]
struct ForgejoView<'a> {
    name: &'a str,
    url: &'a str,
    problem: Option<&'a str>,
    notice: Option<&'a str>,
}

fn forgejo_page(
    state: &AppState,
    headers: &HeaderMap,
    view: &ForgejoView<'_>,
) -> Result<Markup, AppError> {
    let login = user(headers);
    Ok(pages::settings_forgejo(
        &login,
        &settings_nav(state, &login)?,
        &state.store.forgejo_instances()?,
        &state.csrf.issue(&login),
        (view.name, view.url),
        view.problem,
        view.notice,
    ))
}

fn forgejo_refused(
    state: &AppState,
    headers: &HeaderMap,
    view: &ForgejoView<'_>,
) -> Result<Response, AppError> {
    Ok((StatusCode::BAD_REQUEST, forgejo_page(state, headers, view)?).into_response())
}

async fn show_forgejo(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    forgejo_page(&state, &headers, &ForgejoView::default())
}

#[derive(Deserialize)]
struct ForgejoForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
}

/// Registers a Forgejo server after reading its version. A refused name or
/// URL, or a server that does not answer as Forgejo, redraws the page with
/// the reason and stores nothing.
async fn add_forgejo_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ForgejoForm>,
) -> Result<Response, AppError> {
    require_csrf(&state, &user(&headers), &form.csrf)?;
    let (typed_name, typed_url) = (form.name.trim(), form.url.trim());
    let refused = |problem: &str| {
        forgejo_refused(
            &state,
            &headers,
            &ForgejoView {
                name: typed_name,
                url: "",
                problem: Some(problem),
                notice: None,
            },
        )
    };
    let (name, url) = match state
        .store
        .check_new_forgejo_instance(typed_name, typed_url)
    {
        Ok(checked) => checked,
        Err(StoreError::Forgejo(problem)) => return refused(&problem),
        Err(err) => return Err(err.into()),
    };
    let (version, _) = match state.forgejo_probe.version(&url).await {
        Ok(version) => version,
        Err(err) => {
            return refused(&format!(
                "Nothing was added. {url} did not answer as a Forgejo server: {err}"
            ));
        }
    };
    match state.store.add_forgejo_instance(&name, &url, &version) {
        Ok(instance) => {
            let notice = format!("Added {} at {}.", instance.name, instance.base_url);
            Ok(forgejo_page(
                &state,
                &headers,
                &ForgejoView {
                    notice: Some(&notice),
                    ..ForgejoView::default()
                },
            )?
            .into_response())
        }
        Err(StoreError::Forgejo(problem)) => refused(&problem),
        Err(err) => Err(err.into()),
    }
}

#[derive(Deserialize)]
struct CsrfOnlyForm {
    #[serde(default)]
    csrf: String,
}

/// Deletes an instance and its tokens. An instance a project uses stays, and
/// the page names the projects.
async fn delete_forgejo_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<CsrfOnlyForm>,
) -> Result<Response, AppError> {
    require_csrf(&state, &user(&headers), &form.csrf)?;
    if state.store.forgejo_instance(&name)?.is_none() {
        return Err(AppError::NotFound(format!("no Forgejo instance {name:?}")));
    }
    match state.store.delete_forgejo_instance(&name) {
        Ok(()) => {
            let notice = format!("Deleted {name} and the tokens stored for it.");
            Ok(forgejo_page(
                &state,
                &headers,
                &ForgejoView {
                    notice: Some(&notice),
                    ..ForgejoView::default()
                },
            )?
            .into_response())
        }
        Err(StoreError::Forgejo(problem)) => forgejo_refused(
            &state,
            &headers,
            &ForgejoView {
                problem: Some(&problem),
                ..ForgejoView::default()
            },
        ),
        Err(err) => Err(err.into()),
    }
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
struct UseMyAccountForm {
    #[serde(default)]
    on: String,
    #[serde(default)]
    csrf: String,
}

/// The kind of the project's forge, or a bad request for a project on an
/// unknown forge, which has no token to keep.
fn project_forge_kind(project: &Project) -> Result<&ForgeKind, AppError> {
    project
        .forge
        .kind()
        .ok_or_else(|| AppError::BadRequest(project.forge.problem().unwrap_or_default()))
}

async fn save_project_forge_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<ForgeTokenForm>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &name)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    let Some((project, instance)) = state.store.project_with_instance(&name)? else {
        return Err(AppError::UnknownProject(name));
    };
    let kind = project_forge_kind(&project)?;
    let part = pages::PipelinePart::parse(None);
    if !state.secrets.available() {
        let problem = "The master key is missing, so the token was not saved.";
        return project_page(&state, &headers, &name, part, None, Some(problem));
    }
    let token = form.token.trim();
    if token.is_empty() {
        return project_page(&state, &headers, &name, part, Some("Saved."), None);
    }
    let login = user(&headers);
    let saved = forge_tokens::save_for_repo(
        state.secrets.as_ref(),
        state.forges.forge(kind).as_ref(),
        ForgeTokenOwner::Project {
            project: &name,
            forge: kind,
        },
        token,
        &project.repo,
        &ProjectGuard {
            project: &name,
            login: &login,
            instance,
        },
    )
    .await;
    let display = kind.display_name();
    let (notice, problem) = match saved {
        Ok(saved) => {
            let login = &saved.account.login;
            let text = match saved.access {
                Some(Access::Write) => format!("Saved. The project token belongs to {login}."),
                Some(Access::Read) => format!(
                    "Saved. The project token belongs to {login}, which has only Read access to {}. Request jobs cannot push as it, and Renovate triage cannot rerun CI or merge.",
                    project.repo
                ),
                Some(Access::Admin) => format!(
                    "Saved. The project token belongs to {login}, which has Admin access to {}. The design asks for Write, so a ruleset's Admin bypass does not apply to it.",
                    project.repo
                ),
                Some(Access::NotVisible) => unreachable!("a token with no access is not saved"),
                None => format!(
                    "Saved. The project token belongs to {login}. Its access to {} was not checked.",
                    project.repo
                ),
            };
            (Some(text), None)
        }
        Err(SaveError::Rejected) => (
            None,
            Some(format!("{display} rejected the token. Nothing was saved.")),
        ),
        Err(SaveError::Lookup(message)) => (
            None,
            Some(format!(
                "Could not look the account up on {display}: {message}. Nothing was saved."
            )),
        ),
        Err(SaveError::NotVisible(account)) => (
            None,
            Some(format!(
                "The account {} cannot see {} on {display}. Nothing was saved.",
                account.login, project.repo
            )),
        ),
        Err(SaveError::Gone) => {
            if !state.store.is_collaborator(&name, &login)? {
                return Err(AppError::UnknownProject(name));
            }
            (
                None,
                Some("The project changed while the token was checked. Nothing was saved. Save it again.".to_owned()),
            )
        }
        Err(SaveError::InstanceChanged) => (None, Some(STALE_INSTANCE.to_owned())),
        Err(SaveError::Secrets(err)) => return Err(err.into()),
        Err(SaveError::Store(err)) => return Err(err.into()),
    };
    project_page(
        &state,
        &headers,
        &name,
        part,
        notice.as_deref(),
        problem.as_deref(),
    )
}

async fn clear_project_forge_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<ForgeClearForm>,
) -> Result<Markup, AppError> {
    let project = visible_project(&state, &headers, &name)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    let kind = project_forge_kind(&project)?;
    let cleared = forge_tokens::clear(
        state.secrets.as_ref(),
        ForgeTokenOwner::Project {
            project: &name,
            forge: kind,
        },
    );
    match cleared {
        Ok(()) => project_page(
            &state,
            &headers,
            &name,
            pages::PipelinePart::parse(None),
            Some("Cleared."),
            None,
        ),
        Err(SaveError::Secrets(err)) => Err(err.into()),
        Err(SaveError::Store(err)) => Err(err.into()),
        Err(
            SaveError::Rejected
            | SaveError::Lookup(_)
            | SaveError::NotVisible(_)
            | SaveError::Gone
            | SaveError::InstanceChanged,
        ) => {
            unreachable!("clearing makes no lookup")
        }
    }
}

/// Sets the switch of the request's own login. The form names no login, so
/// one collaborator cannot change another's switch.
async fn set_use_my_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<UseMyAccountForm>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &name)?;
    let login = user(&headers);
    require_csrf(&state, &login, &form.csrf)?;
    let on = form.on == "on";
    state.store.set_use_my_account(&name, &login, on)?;
    let notice = if on {
        "Saved. \"Use my account\" is on for you in this project. Jobs do not use it yet."
    } else {
        "Saved. \"Use my account\" is off for you in this project. Jobs do not use it yet."
    };
    project_page(
        &state,
        &headers,
        &name,
        pages::PipelinePart::parse(None),
        Some(notice),
        None,
    )
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
    #[serde(default)]
    upstream_forge: String,
    #[serde(default)]
    renovate_author: String,
    /// The other fields, among them `server_<forge>`, the registration number of
    /// each Forgejo instance the card showed.
    #[serde(flatten)]
    fields: HashMap<String, String>,
}

/// The registration number the form showed for `forge`, for a Forgejo instance.
fn shown_for<'a>(fields: &'a HashMap<String, String>, forge: &ForgeKind) -> Option<&'a str> {
    fields
        .get(&format!("server_{}", forge.as_str()))
        .map(String::as_str)
}

fn renovate_account_well_formed(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn path_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

struct MaintenanceDraft {
    maintenance: Maintenance,
    refusal: Option<String>,
    /// The selected upstream forge is not registered or is not the instance
    /// the form showed, so no forge call may use it.
    upstream_unusable: bool,
}

fn maintenance_from_form(
    state: &AppState,
    form: &MaintenanceForm,
    forge: Option<&ForgeKind>,
) -> Result<MaintenanceDraft, StoreError> {
    let kind = if form.packaging_type.is_empty() {
        Some(PackagingType::default())
    } else {
        PackagingType::parse(&form.packaging_type)
    };
    let own_forge = forge.cloned().unwrap_or(ForgeKind::GitHub);
    let (upstream_forge, unusable) = match form.upstream_forge.as_str() {
        "" => (Some(own_forge.clone()), None),
        name => match state.known_forge(name)? {
            None => (
                None,
                Some(format!(
                    "The upstream forge {name:?} is not registered. Pick another upstream forge."
                )),
            ),
            Some(kind) if !shown_server(&kind, shown_for(&form.fields, &kind)) => (
                None,
                Some(format!(
                    "The upstream forge {name:?} changed since this page loaded. Nothing was saved or checked. Reload the page."
                )),
            ),
            Some(kind) => (Some(kind), None),
        },
    };
    let packaging = form.packaging.is_some().then(|| Packaging {
        kind: kind.unwrap_or_default(),
        manifest_files: path_lines(&form.manifest_files),
        build_config_files: path_lines(&form.build_config_files),
        upstream_repo: form.upstream_repo.trim().to_owned(),
        upstream_forge: upstream_forge.clone().unwrap_or(own_forge),
    });
    let salsa = forge == Some(&ForgeKind::Salsa);
    let author = form.renovate_author.trim();
    let maintenance = Maintenance {
        renovate: form.renovate.is_some(),
        renovate_author: (salsa && !author.is_empty()).then(|| author.to_owned()),
        packaging,
    };
    let refusal = match (&maintenance.packaging, kind) {
        (_, _) if salsa && maintenance.renovate && author.is_empty() => Some(
            "Give the Renovate account: the Salsa username Renovate opens its merge requests as."
                .to_owned(),
        ),
        (_, _) if salsa && !renovate_account_well_formed(author) => Some(
            "The Renovate account must be a Salsa username, with no spaces or slashes.".to_owned(),
        ),
        (Some(_), None) => Some("The packaging type must be snap, deb or other.".to_owned()),
        (Some(_), _) if upstream_forge.is_none() => unusable.clone(),
        _ => maintenance.check().err(),
    };
    Ok(MaintenanceDraft {
        upstream_unusable: maintenance.packaging.is_some() && unusable.is_some(),
        maintenance,
        refusal,
    })
}

struct SaveChecks {
    notice: String,
    snapshots: Vec<TokenRow>,
    check: Option<pages::UpstreamCheckView>,
}

/// Runs the forge checks a Maintenance save needs before it writes. `Err`
/// holds the refusal and the upstream report to draw with it.
async fn save_checks(
    state: &AppState,
    project: &str,
    record: &Project,
    maintenance: &Maintenance,
) -> Result<Result<SaveChecks, (String, Option<pages::UpstreamCheckView>)>, AppError> {
    let mut notice = "Saved.".to_owned();
    let mut snapshots = Vec::new();
    if maintenance.renovate {
        let project_forge = project_forge_kind(record)?;
        match renovate_access(state, project, record).await? {
            RenovateCheck::Refused(problem) => return Ok(Err((problem, None))),
            RenovateCheck::Warned { warning, sealed } => {
                notice = format!("{notice} {warning}");
                snapshots.push(TokenRow {
                    upstream: false,
                    forge: project_forge.clone(),
                    sealed: Some(sealed),
                });
            }
            RenovateCheck::Passed { sealed } => snapshots.push(TokenRow {
                upstream: false,
                forge: project_forge.clone(),
                sealed: Some(sealed),
            }),
        }
    }
    let check = match &maintenance.packaging {
        Some(packaging) => {
            let (view, rows) = check_upstream(state, project, record, packaging).await;
            snapshots.extend(rows);
            Some(view)
        }
        None => None,
    };
    if let Some(report) = &check {
        match report.readable {
            Some(false) => {
                let problem = format!("{} Nothing was saved.", report.text);
                return Ok(Err((problem, check)));
            }
            Some(true) => {}
            None => notice = format!("{notice} {}", report.text),
        }
    }
    Ok(Ok(SaveChecks {
        notice,
        snapshots,
        check,
    }))
}

const STALE_PROJECT: &str =
    "The project changed while the access was checked. Nothing was saved. Save it again.";
const STALE_TOKEN: &str =
    "A token changed while the access was checked. Nothing was saved. Save it again.";

fn snapshot_credentials<'a>(
    project: &'a str,
    snapshots: &'a [TokenRow],
) -> Vec<(ForgeTokenOwner<'a>, Option<&'a [u8]>)> {
    snapshots
        .iter()
        .map(|snapshot| {
            let owner = if snapshot.upstream {
                ForgeTokenOwner::Upstream {
                    project,
                    forge: &snapshot.forge,
                }
            } else {
                ForgeTokenOwner::Project {
                    project,
                    forge: &snapshot.forge,
                }
            };
            (owner, snapshot.sealed.as_deref())
        })
        .collect()
}

async fn save_project_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<MaintenanceForm>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &project)?;
    let Some((record, instance)) = state.store.project_with_instance(&project)? else {
        return Err(AppError::UnknownProject(project));
    };
    let part = pages::PipelinePart::parse(None);
    let MaintenanceDraft {
        maintenance,
        refusal,
        ..
    } = maintenance_from_form(&state, &form, record.forge.kind())?;
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
    let checks = match save_checks(&state, &project, &record, &maintenance).await? {
        Ok(checks) => checks,
        Err((problem, report)) => {
            return project_page_full(
                &state,
                &headers,
                &project,
                part,
                None,
                Some(&problem),
                Some(maintenance),
                report,
            );
        }
    };
    let SaveChecks {
        notice,
        snapshots,
        check,
    } = checks;
    let login = user(&headers);
    let (stored, unknown_upstream) = state.store.project_maintenance_lenient(&project)?;
    let previous_upstream = match unknown_upstream {
        Some(_) => None,
        None => stored.packaging.map(|packaging| packaging.upstream_forge),
    };
    let credentials = snapshot_credentials(&project, &snapshots);
    let saved = state.store.save_project_maintenance_guarded(
        &ProjectGuard {
            project: &project,
            login: &login,
            instance,
        },
        &maintenance,
        &credentials,
    )?;
    let problem = match saved {
        MaintenanceSave::Saved => {
            drop_stale_upstream_token(&state, &project, previous_upstream.as_ref(), &maintenance)?;
            return project_page_full(
                &state,
                &headers,
                &project,
                part,
                Some(&notice),
                None,
                None,
                check,
            );
        }
        MaintenanceSave::Gone if !state.store.is_collaborator(&project, &login)? => {
            return Err(AppError::UnknownProject(project));
        }
        MaintenanceSave::Gone => STALE_PROJECT,
        MaintenanceSave::TokenChanged => STALE_TOKEN,
        MaintenanceSave::InstanceChanged => STALE_INSTANCE,
    };
    project_page_with(
        &state,
        &headers,
        &project,
        part,
        None,
        Some(problem),
        Some(maintenance),
    )
}

/// The stored upstream token is bound to its forge, so it does not carry
/// over when the upstream moves to another forge.
fn drop_stale_upstream_token(
    state: &AppState,
    project: &str,
    before: Option<&ForgeKind>,
    saved: &Maintenance,
) -> Result<(), AppError> {
    match (before, &saved.packaging) {
        (Some(before), Some(packaging)) if *before != packaging.upstream_forge => {
            clear_upstream_token(state, project, before)
        }
        _ => Ok(()),
    }
}

fn clear_upstream_token(
    state: &AppState,
    project: &str,
    forge: &ForgeKind,
) -> Result<(), AppError> {
    match forge_tokens::clear(
        state.secrets.as_ref(),
        ForgeTokenOwner::Upstream { project, forge },
    ) {
        Ok(()) => Ok(()),
        Err(SaveError::Secrets(err)) => Err(err.into()),
        Err(SaveError::Store(err)) => Err(err.into()),
        Err(_) => unreachable!("clearing makes no lookup"),
    }
}

/// Reads the upstream repository the way a packaging job would, with the
/// credential a job would pick, and says what happened. `readable` is `None`
/// when the check could not run.
async fn check_upstream(
    state: &AppState,
    name: &str,
    project: &Project,
    packaging: &Packaging,
) -> (pages::UpstreamCheckView, Vec<TokenRow>) {
    let forge = &packaging.upstream_forge;
    let repo = &packaging.upstream_repo;
    let display = forge.display_name();
    let accounts = Accounts::new(
        Arc::clone(&state.store),
        Arc::clone(&state.secrets),
        state.forges.clone(),
    );
    let read = match accounts.upstream(name, project, forge) {
        Ok(read) => read,
        Err(ResolveError::Account(why)) => {
            return (
                unchecked_upstream(&format!("the credential could not be read: {why}")),
                Vec::new(),
            );
        }
        Err(ResolveError::Store(err)) => {
            return (
                unchecked_upstream(&format!("the stored tokens could not be read: {err}")),
                Vec::new(),
            );
        }
    };
    let used = match &read.credential {
        UpstreamCredential::Upstream(account) => {
            format!("the upstream token of {}", account.login)
        }
        UpstreamCredential::Project(account) => {
            format!("the project token of {}", account.login)
        }
        UpstreamCredential::None => "no token".to_owned(),
    };
    let advice = "Save an upstream token whose account can read the repository, or correct the upstream forge or path.";
    let (readable, text) = match state.forges.forge(forge).access(&read.account, repo).await {
        Ok(Access::NotVisible) => (
            Some(false),
            format!("Checked with {used}: {repo} could not be read on {display}. {advice}"),
        ),
        Ok(_) => {
            let mut text = format!("Checked with {used}: {repo} is readable on {display}.");
            if *forge == ForgeKind::GitHub && read.credential == UpstreamCredential::None {
                text.push_str(" GitHub rate limits reads with no token, and an upstream token lifts the limit.");
            }
            (Some(true), text)
        }
        Err(err) if err.starts_with(factory_runner::REJECTED) => (
            Some(false),
            format!("Checked with {used}: {display} rejected the token. {advice}"),
        ),
        Err(err) => {
            return (
                unchecked_upstream(&format!("the read with {used} failed: {err}")),
                read.rows,
            );
        }
    };
    (pages::UpstreamCheckView { readable, text }, read.rows)
}

fn unchecked_upstream(why: &str) -> pages::UpstreamCheckView {
    pages::UpstreamCheckView {
        readable: None,
        text: format!("The upstream was not checked: {why}"),
    }
}

/// Tries the upstream repository from the form's fields and stores nothing.
async fn check_project_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<MaintenanceForm>,
) -> Result<Markup, AppError> {
    let record = visible_project(&state, &headers, &project)?;
    let part = pages::PipelinePart::parse(None);
    let MaintenanceDraft {
        maintenance,
        refusal,
        upstream_unusable,
    } = maintenance_from_form(&state, &form, record.forge.kind())?;
    let Some(packaging) = maintenance.packaging.clone() else {
        return project_page_with(
            &state,
            &headers,
            &project,
            part,
            None,
            Some("Tick Packaging project to check its upstream."),
            Some(maintenance),
        );
    };
    // A refusal about another field does not stop the connection check.
    let malformed = !packaging
        .upstream_forge
        .valid_repo(&packaging.upstream_repo);
    if malformed || upstream_unusable {
        let problem = refusal.unwrap_or_default();
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
    let (report, _) = check_upstream(&state, &project, &record, &packaging).await;
    project_page_full(
        &state,
        &headers,
        &project,
        part,
        None,
        None,
        Some(maintenance),
        Some(report),
    )
}

async fn save_upstream_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<TokenForgeQuery>,
    Form(form): Form<UpstreamTokenForm>,
) -> Result<Markup, AppError> {
    let project = visible_project(&state, &headers, &name)?;
    let login = user(&headers);
    require_csrf(&state, &login, &form.csrf)?;
    let kind = token_forge(&state, &query)?;
    let part = pages::PipelinePart::parse(None);
    let draft = maintenance_from_form(&state, &form.draft(), project.forge.kind())?.maintenance;
    let redraw = |notice: Option<&str>, problem: Option<&str>| {
        project_page_full(
            &state,
            &headers,
            &name,
            part,
            notice,
            problem,
            Some(draft.clone()),
            None,
        )
    };
    if !shown_server(&kind, shown_for(&form.fields, &kind)) {
        return redraw(None, Some(STALE_FORM));
    }
    if !state.secrets.available() {
        return redraw(
            None,
            Some("The master key is missing, so the token was not saved."),
        );
    }
    let token = form.token(&kind).trim();
    if token.is_empty() {
        return redraw(Some("Saved."), None);
    }
    let Some((_, instance)) = state.store.project_with_instance(&name)? else {
        return Err(AppError::UnknownProject(name));
    };
    let display = kind.display_name();
    let saved = forge_tokens::save_guarded(
        state.secrets.as_ref(),
        state.forges.forge(&kind).as_ref(),
        ForgeTokenOwner::Upstream {
            project: &name,
            forge: &kind,
        },
        token,
        &ProjectGuard {
            project: &name,
            login: &login,
            instance,
        },
    )
    .await;
    let (notice, problem) = match saved {
        Ok(account) => (
            Some(format!(
                "Saved. The upstream token belongs to {}.",
                account.login
            )),
            None,
        ),
        Err(SaveError::Rejected) => (
            None,
            Some(format!("{display} rejected the token. Nothing was saved.")),
        ),
        Err(SaveError::Lookup(message)) => (
            None,
            Some(format!(
                "Could not look the account up on {display}: {message}. Nothing was saved."
            )),
        ),
        Err(SaveError::Gone) => {
            if !state.store.is_collaborator(&name, &login)? {
                return Err(AppError::UnknownProject(name));
            }
            (None, Some(STALE_PROJECT.to_owned()))
        }
        Err(SaveError::InstanceChanged) => (None, Some(STALE_INSTANCE.to_owned())),
        Err(SaveError::Secrets(err)) => return Err(err.into()),
        Err(SaveError::Store(err)) => return Err(err.into()),
        Err(SaveError::NotVisible(_)) => {
            unreachable!("an upstream token is saved with no repository check")
        }
    };
    redraw(notice.as_deref(), problem.as_deref())
}

async fn clear_upstream_token_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<TokenForgeQuery>,
    Form(form): Form<UpstreamTokenForm>,
) -> Result<Markup, AppError> {
    let project = visible_project(&state, &headers, &name)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    let kind = token_forge(&state, &query)?;
    let draft = maintenance_from_form(&state, &form.draft(), project.forge.kind())?.maintenance;
    if !shown_server(&kind, shown_for(&form.fields, &kind)) {
        return project_page_full(
            &state,
            &headers,
            &name,
            pages::PipelinePart::parse(None),
            None,
            Some(STALE_FORM),
            Some(draft),
            None,
        );
    }
    clear_upstream_token(&state, &name, &kind)?;
    project_page_full(
        &state,
        &headers,
        &name,
        pages::PipelinePart::parse(None),
        Some("Cleared."),
        None,
        Some(draft),
        None,
    )
}

enum RenovateCheck {
    Refused(String),
    Warned { warning: String, sealed: Vec<u8> },
    Passed { sealed: Vec<u8> },
}

/// Reads the project account's access to the repository for Renovate triage.
async fn renovate_access(
    state: &AppState,
    name: &str,
    project: &Project,
) -> Result<RenovateCheck, AppError> {
    let kind = project_forge_kind(project)?;
    let owner = ForgeTokenOwner::Project {
        project: name,
        forge: kind,
    };
    let missing = || {
        RenovateCheck::Refused(format!(
            "Renovate pull requests need a project token. Save one in the Forge account card on /settings/projects/{}#forge-account.",
            query_escape(name)
        ))
    };
    let unchecked = |account: ForgeAccount, sealed: Vec<u8>| RenovateCheck::Warned {
        warning: format!(
            "The access of {} to {} was not checked.",
            account.login, project.repo
        ),
        sealed,
    };
    // Read before the token, so a token replaced in between fails the write guard.
    let (row_project, header) = owner.row();
    let Some((_, sealed)) = state.store.get_secret(SecretRef {
        project: &row_project,
        server: "",
        header: &header,
    })?
    else {
        return Ok(missing());
    };
    let (token, found) = match state.secrets.forge_token_with_account(owner) {
        Ok(Some(stored)) => stored,
        Ok(None) => return Ok(missing()),
        Err(_) => {
            return Ok(match state.store.forge_account(owner)? {
                Some(account) => unchecked(account, sealed),
                None => missing(),
            });
        }
    };
    let forge = state.forges.forge(kind);
    let account = forge.account(token, &found);
    let display = kind.display_name();
    Ok(match forge.access(&account, &project.repo).await {
        Ok(Access::NotVisible) => RenovateCheck::Refused(format!(
            "The project account {} cannot see {} on {display}. Renovate was not turned on.",
            found.login, project.repo
        )),
        Ok(Access::Read) => RenovateCheck::Warned {
            warning: format!(
                "The project account {} has only Read access to {}, so the factory cannot rerun failed CI and cannot merge Renovate pull requests, even with auto-merge on.",
                found.login, project.repo
            ),
            sealed,
        },
        Ok(Access::Write | Access::Admin) => RenovateCheck::Passed { sealed },
        Err(_) => unchecked(found, sealed),
    })
}

#[derive(Deserialize)]
struct ChecksForm {
    #[serde(default)]
    checks: String,
    #[serde(default)]
    csrf: String,
}

async fn save_project_checks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<ChecksForm>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &project)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    let checks = form.checks.trim();
    state
        .store
        .save_project_checks(&project, (!checks.is_empty()).then_some(checks))?;
    project_page(
        &state,
        &headers,
        &project,
        pages::PipelinePart::parse(None),
        Some("Saved."),
        None,
    )
}

#[derive(Deserialize)]
struct PackagesForm {
    #[serde(default)]
    packages: String,
    #[serde(default)]
    csrf: String,
}

async fn save_project_packages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<PackagesForm>,
) -> Result<Markup, AppError> {
    visible_project(&state, &headers, &project)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    let packages =
        crate::packages::validate(form.packages.lines()).map_err(AppError::BadRequest)?;
    state.store.save_project_packages(&project, &packages)?;
    project_page(
        &state,
        &headers,
        &project,
        pages::PipelinePart::parse(None),
        Some("Saved."),
        None,
    )
}

#[derive(Deserialize)]
struct NameForm {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    csrf: String,
}

/// The display name to store for `project` from what the form held: `None`
/// clears it, `Err` is the problem to show.
fn display_name_value(project: &str, raw: &str) -> Result<Option<String>, String> {
    let value = raw.trim();
    if value.is_empty() || value == project {
        return Ok(None);
    }
    if value.chars().count() > 64 {
        return Err("A project name is at most 64 characters.".to_owned());
    }
    if value.chars().any(char::is_control) {
        return Err("A project name has no control characters.".to_owned());
    }
    Ok(Some(value.to_owned()))
}

async fn save_project_name(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Form(form): Form<NameForm>,
) -> Result<Response, AppError> {
    visible_project(&state, &headers, &project)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    // Onboarding holds this lock from its name check to its insert.
    let _one_onboarding_at_a_time = state.onboarding.lock().await;
    visible_project(&state, &headers, &project)?;
    let problem = match display_name_value(&project, &form.display_name) {
        Ok(value) => match state
            .store
            .save_project_display_name(&project, value.as_deref())
        {
            Ok(()) => {
                return Ok(Redirect::to(&format!(
                    "/settings/projects/{}?notice=Saved.",
                    query_escape(&project)
                ))
                .into_response());
            }
            Err(StoreError::DuplicateName(name)) => {
                format!("A project named {name:?} already exists.")
            }
            Err(err) => return Err(err.into()),
        },
        Err(problem) => problem,
    };
    Ok(project_page(
        &state,
        &headers,
        &project,
        pages::PipelinePart::parse(None),
        None,
        Some(&problem),
    )?
    .into_response())
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
    let shown = project_label(&state, &project)?.shown;
    if form.confirm != shown {
        let problem = format!("Nothing was removed. Type {shown} to remove the project.");
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
    let notice = format!("Removed project {shown}.");
    Ok(Redirect::to(&format!("/settings?notice={}", query_escape(&notice))).into_response())
}

/// One server's settings as a project's page for it shows them.
fn server_settings(
    state: &AppState,
    project: &str,
    server: McpServer,
) -> Result<ServerSettings, StoreError> {
    let saved = state.store.project_mcp(project, &server.name)?;
    let stdio = match server.as_stdio() {
        Some((command, args)) => Some(pages::StdioSettings {
            command: command.to_owned(),
            args: args.to_vec(),
            extra_args: state.store.project_mcp_args(project, &server.name)?,
        }),
        None => None,
    };
    let declared = server.as_remote().map_or(&[][..], |(_, headers)| headers);
    let headers = declared
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
        stdio,
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
        &project_label(state, project)?,
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
/// header values and its tools, or for a stdio server its extra arguments. Everything is checked before any write, so a
/// rejected submit changes nothing. An enabled server needs a stage, and a
/// header value cannot be set while the secret store is unavailable.
async fn save_project_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, server)): Path<(String, String)>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Markup, AppError> {
    let registered = project_and_server(&state, &headers, &project, &server)?;
    let mut extra_args = Vec::new();
    if registered.as_stdio().is_some() {
        for (key, value) in &pairs {
            match key.as_str() {
                "arg" => extra_args.push(value.clone()),
                "arg_escaped" => extra_args.push(unescape_arg(value)?),
                _ => {}
            }
        }
    }
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
    let extra_args = if registered.as_stdio().is_some() {
        Some(crate::mcp::validate_args(&extra_args)?)
    } else {
        None
    };
    let value = |header: &str| {
        form.get(&format!("{base}.header.{header}"))
            .filter(|value| !value.is_empty())
    };
    let registered_headers = registered
        .as_remote()
        .map_or(&[][..], |(_, headers)| headers);
    if !state.secrets.available()
        && registered_headers
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
    if let Some(args) = &extra_args {
        state
            .store
            .save_project_mcp_args(&project, &registered.name, args)?;
    }
    for header in registered_headers {
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

/// An MCP server form: a `kind`, the remote fields `url` and one `header` field
/// per header-name row, and the stdio fields `command` and one `arg` field per
/// argument row. Rows post as repeated fields, so the form parses as a list of
/// pairs rather than a struct. The Add form also posts a `name`; the edit route
/// takes the name from its path and ignores any posted one.
struct McpServerForm {
    name: String,
    /// Whether the posted kind is `stdio`, when one was posted. The edit route
    /// only checks it against the stored kind, because the kind is fixed.
    stdio: Option<bool>,
    url: String,
    headers: Vec<String>,
    command: String,
    args: Vec<String>,
    /// Set when the operator confirmed an edit that deletes stored values.
    confirm: bool,
}

/// Reads an argument the edit form escaped so that a one-line input can carry
/// it: `\\` is a backslash, `\n` a line feed and `\r` a carriage return.
fn unescape_arg(value: &str) -> Result<String, McpError> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            _ => {
                return Err(McpError::BadArgs(
                    "an escaped argument has a backslash that is not \\\\, \\n or \\r".to_owned(),
                ));
            }
        }
    }
    Ok(out)
}

impl McpServerForm {
    /// Reads the posted pairs. Each header name is trimmed and an empty one,
    /// from a blank row, is dropped. An argument is kept as typed, because a
    /// space in it is part of it; the store drops an empty one. The store
    /// validates both lists.
    fn from_pairs(pairs: Vec<(String, String)>) -> Result<Self, McpError> {
        let mut form = McpServerForm {
            name: String::new(),
            stdio: None,
            url: String::new(),
            headers: Vec::new(),
            command: String::new(),
            args: Vec::new(),
            confirm: false,
        };
        for (key, value) in pairs {
            match key.as_str() {
                "name" => form.name = value,
                "kind" => {
                    form.stdio = Some(match value.as_str() {
                        "stdio" => true,
                        "remote" => false,
                        _ => {
                            return Err(McpError::BadKind(format!(
                                "the kind {value:?} is not remote or stdio"
                            )));
                        }
                    });
                }
                "url" => form.url = value,
                "command" => form.command = value,
                "confirm" => form.confirm = true,
                "arg" => form.args.push(value),
                "arg_escaped" => form.args.push(unescape_arg(&value)?),
                "header" => {
                    let name = value.trim();
                    if !name.is_empty() {
                        form.headers.push(name.to_owned());
                    }
                }
                _ => {}
            }
        }
        Ok(form)
    }

    /// The transport the posted fields describe, ignoring the other kind's.
    fn transport(&self, stdio: bool) -> McpTransport {
        if stdio {
            McpTransport::Stdio {
                command: self.command.clone(),
                args: self.args.clone(),
            }
        } else {
            McpTransport::Remote {
                url: self.url.trim().to_owned(),
                headers: self.headers.clone(),
            }
        }
    }
}

async fn add_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Markup, AppError> {
    let form = McpServerForm::from_pairs(pairs)?;
    let transport = form.transport(form.stdio == Some(true));
    state
        .store
        .create_mcp_server_with(form.name.trim(), &transport)?;
    mcp_server_page(&state, &headers, form.name.trim(), Some("Added."))
}

async fn edit_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Markup, AppError> {
    let form = McpServerForm::from_pairs(pairs)?;
    let Some(registered) = state.store.mcp_server(&name)? else {
        return Err(McpError::NotFound(name).into());
    };
    let stdio = registered.as_stdio().is_some();
    if form.stdio.is_some_and(|posted| posted != stdio) {
        return Err(McpError::KindChange(name).into());
    }
    let transport = form.transport(stdio);
    // An edit that deletes stored values waits for the operator to confirm it.
    // A stdio server has none, so it saves at once.
    if let McpTransport::Remote {
        url,
        headers: names,
    } = &transport
    {
        let deletes = state.store.mcp_edit_deletes(&name, url, names)?;
        if !deletes.is_empty() && !form.confirm {
            let total = deletes.len();
            let visible = project_labels(&state, &user(&headers))?;
            let shown = deletes
                .into_iter()
                .filter_map(|(project, header)| {
                    let label = visible.iter().find(|label| label.id == project)?;
                    Some((label.clone(), header))
                })
                .collect();
            let pending = pages::PendingEdit {
                url: url.clone(),
                headers: names.clone(),
                deletes: shown,
                total,
            };
            return mcp_server_page_with(&state, &headers, &name, None, Some(&pending), None);
        }
    }
    state.store.edit_mcp_server_with(&name, &transport)?;
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
    // A stdio server runs inside the worker, so the factory cannot reach it.
    if registered.as_stdio().is_some() {
        return Err(AppError::BadRequest(format!(
            "the server {server:?} runs inside the worker, so its connection cannot be checked"
        )));
    }
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
    let registered_headers = registered
        .as_remote()
        .map_or(&[][..], |(_, headers)| headers);
    let missing: Vec<String> = registered_headers
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

async fn save_polling(
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
    polling_page(&state, &headers, Some("Saved."))
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

fn require_csrf(state: &AppState, login: &str, token: &str) -> Result<(), AppError> {
    if state.csrf.accepts(login, token) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "the form is missing its token or it is stale, reload the page".into(),
        ))
    }
}

fn forge_kind(state: &AppState, name: &str) -> Result<ForgeKind, AppError> {
    state
        .known_forge(name)?
        .ok_or_else(|| AppError::NotFound(format!("no forge {name}")))
}

fn forge_page(
    state: &AppState,
    headers: &HeaderMap,
    kind: &ForgeKind,
    notice: Option<&str>,
) -> Result<Markup, AppError> {
    let login = user(headers);
    let account = state.store.forge_account(ForgeTokenOwner::User {
        login: &login,
        forge: kind,
    })?;
    Ok(pages::settings_forge(
        &login,
        &settings_nav(state, &login)?,
        kind,
        account.as_ref(),
        state.secrets.available(),
        &state.csrf.issue(&login),
        notice,
    ))
}

async fn show_forge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(forge): Path<String>,
) -> Result<Markup, AppError> {
    forge_page(&state, &headers, &forge_kind(&state, &forge)?, None)
}

async fn save_forge_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(forge): Path<String>,
    Form(form): Form<ForgeTokenForm>,
) -> Result<Markup, AppError> {
    let kind = forge_kind(&state, &forge)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    if !shown_server(&kind, Some(&form.server)) {
        return forge_page(&state, &headers, &kind, Some(STALE_FORM));
    }
    if !state.secrets.available() {
        return forge_page(
            &state,
            &headers,
            &kind,
            Some("The master key is missing, so the token was not saved."),
        );
    }
    let token = form.token.trim();
    if token.is_empty() {
        return forge_page(&state, &headers, &kind, Some("Saved."));
    }
    let login = user(&headers);
    let owner = ForgeTokenOwner::User {
        login: &login,
        forge: &kind,
    };
    let saved = forge_tokens::save(
        state.secrets.as_ref(),
        state.forges.forge(&kind).as_ref(),
        owner,
        token,
    )
    .await;
    let notice = match saved {
        Ok(account) => format!("Saved. The token belongs to {}.", account.login),
        Err(SaveError::Rejected) => format!(
            "{} rejected the token. Nothing was saved.",
            kind.display_name()
        ),
        Err(SaveError::Lookup(message)) => format!(
            "Could not look the account up on {}: {message}. Nothing was saved.",
            kind.display_name()
        ),
        Err(SaveError::Secrets(err)) => return Err(err.into()),
        Err(SaveError::Store(err)) => return Err(err.into()),
        Err(SaveError::NotVisible(_) | SaveError::Gone) => {
            unreachable!("a user token is saved with no repository or project")
        }
        Err(SaveError::InstanceChanged) => STALE_INSTANCE.to_owned(),
    };
    forge_page(&state, &headers, &kind, Some(&notice))
}

async fn clear_forge_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(forge): Path<String>,
    Form(form): Form<ForgeClearForm>,
) -> Result<Markup, AppError> {
    let kind = forge_kind(&state, &forge)?;
    require_csrf(&state, &user(&headers), &form.csrf)?;
    if !shown_server(&kind, Some(&form.server)) {
        return forge_page(&state, &headers, &kind, Some(STALE_FORM));
    }
    let login = user(&headers);
    let cleared = forge_tokens::clear(
        state.secrets.as_ref(),
        ForgeTokenOwner::User {
            login: &login,
            forge: &kind,
        },
    );
    match cleared {
        Ok(()) => forge_page(&state, &headers, &kind, Some("Cleared.")),
        Err(SaveError::Secrets(err)) => Err(err.into()),
        Err(SaveError::Store(err)) => Err(err.into()),
        Err(
            SaveError::Rejected
            | SaveError::Lookup(_)
            | SaveError::NotVisible(_)
            | SaveError::Gone
            | SaveError::InstanceChanged,
        ) => {
            unreachable!("clearing makes no lookup")
        }
    }
}

async fn show_copilot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Markup, AppError> {
    copilot_page(&state, &headers, None).await
}

#[derive(Deserialize)]
struct ForgeTokenForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    csrf: String,
    /// The registration number of the Forgejo instance the page showed.
    #[serde(default)]
    server: String,
}

const STALE_INSTANCE: &str = "The Forgejo instance was deleted or registered again while this was checked. Nothing was saved. Reload the page.";
const STALE_ONBOARD: &str = "The Forgejo instance was deleted or registered again while the checks ran. No project was created. Reload the page.";
const STALE_ONBOARD_FORM: &str = "The Forgejo instance changed since this page loaded. No check ran and no project was created. Reload the page.";
const STALE_FORM: &str = "The Forgejo instance changed since this page loaded. Nothing was saved or sent. Reload the page.";

/// Whether a form shown for `kind` names the registration the instance has
/// now. A token typed for one server must not reach another that took over
/// the name, even at the same URL.
fn shown_server(kind: &ForgeKind, shown: Option<&str>) -> bool {
    match kind {
        ForgeKind::Forgejo(server) => shown == Some(server.registration.to_string().as_str()),
        ForgeKind::GitHub | ForgeKind::Salsa => true,
    }
}

/// The upstream token forms carry the Maintenance card's current fields, so a
/// token saved or cleared before the card is saved redraws the same draft and
/// acts on the upstream forge chosen there.
#[derive(Deserialize)]
struct UpstreamTokenForm {
    #[serde(default)]
    csrf: String,
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
    #[serde(default)]
    upstream_forge: String,
    #[serde(default)]
    renovate_author: String,
    /// The `token_<forge>` fields, one per forge, and the `server_<forge>`
    /// fields.
    #[serde(flatten)]
    fields: HashMap<String, String>,
}

/// The forge a token button names in its `formaction`.
#[derive(Deserialize)]
struct TokenForgeQuery {
    forge: String,
}

fn token_forge(state: &AppState, query: &TokenForgeQuery) -> Result<ForgeKind, AppError> {
    state
        .known_forge(&query.forge)?
        .ok_or_else(|| AppError::BadRequest(format!("{:?} is not a forge", query.forge)))
}

impl UpstreamTokenForm {
    fn token(&self, forge: &ForgeKind) -> &str {
        self.fields
            .get(&format!("token_{}", forge.as_str()))
            .map_or("", String::as_str)
    }

    fn draft(&self) -> MaintenanceForm {
        MaintenanceForm {
            renovate: self.renovate.clone(),
            packaging: self.packaging.clone(),
            packaging_type: self.packaging_type.clone(),
            manifest_files: self.manifest_files.clone(),
            build_config_files: self.build_config_files.clone(),
            upstream_repo: self.upstream_repo.clone(),
            upstream_forge: self.upstream_forge.clone(),
            renovate_author: self.renovate_author.clone(),
            fields: self.fields.clone(),
        }
    }
}

#[derive(Deserialize)]
struct ForgeClearForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    server: String,
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
    /// The forge's stored name. Empty means GitHub.
    #[serde(default)]
    forge: String,
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
    /// The other fields, among them `server_<forge>`, the registration number
    /// of each Forgejo instance the page showed.
    #[serde(flatten)]
    fields: HashMap<String, String>,
}

async fn show_onboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NoticeQuery>,
) -> Result<Markup, AppError> {
    let view = pages::OnboardView {
        forges: state.forges()?,
        forge: ForgeKind::GitHub,
        name: "",
        repo: "",
        base: DEFAULT_BASE,
        checks: "",
        models: &StageModels::default(),
        auto_merge: false,
        llm_review: false,
        token_page: None,
    };
    Ok(pages::onboard(
        &user(&headers),
        &view,
        None,
        query.notice.as_deref(),
    ))
}

/// Runs the checks, and on `create` or `skip` with no error, onboards the
/// project and redirects to its board. The checks run again here, not only in
/// the earlier `check` step, so a project is never created while a required
/// check fails and a rejection writes no row.
/// Why the wizard refuses the posted forge before any forge call: it is not
/// one the factory supports, or it is a Forgejo instance that changed since
/// the page was drawn.
fn onboard_refusal(
    forge: Option<&ForgeKind>,
    forge_field: &str,
    fields: &HashMap<String, String>,
) -> Option<String> {
    match forge {
        None => Some(format!(
            "The forge {forge_field:?} is not one the factory supports."
        )),
        Some(kind) if !shown_server(kind, shown_for(fields, kind)) => {
            Some(STALE_ONBOARD_FORM.to_owned())
        }
        Some(_) => None,
    }
}

async fn submit_onboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<OnboardForm>,
) -> Result<Response, AppError> {
    let forge_field = form.forge.trim();
    let parsed_forge = if forge_field.is_empty() {
        Some(ForgeKind::GitHub)
    } else {
        state.known_forge(forge_field)?
    };
    let forge = parsed_forge.clone().unwrap_or(ForgeKind::GitHub);
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

    let refusal = onboard_refusal(parsed_forge.as_ref(), forge_field, &form.fields);
    if let Some(message) = refusal {
        let report = RepoReport {
            errors: vec![message],
            warnings: Vec::new(),
        };
        let view = pages::OnboardView {
            forges: state.forges()?,
            forge,
            name,
            repo,
            base: base_field,
            checks,
            models: &models,
            auto_merge,
            llm_review,
            token_page: None,
        };
        return Ok(pages::onboard(&user(&headers), &view, Some(&report), None).into_response());
    }

    let _one_onboarding_at_a_time = state.onboarding.lock().await;
    let (mut report, token_page) =
        onboard_report(&state, &headers, effective_name, &forge, repo, base).await?;

    if matches!(form.action.as_str(), "create" | "skip") && report.errors.is_empty() {
        clear_leftover_jobs(&state, effective_name, &mut report).await?;
    }
    if matches!(form.action.as_str(), "create" | "skip") && report.errors.is_empty() {
        let checks = (!checks.is_empty()).then_some(checks);
        match state.store.create_project_as(
            effective_name,
            &forge,
            repo,
            base,
            checks,
            Some(&user(&headers)),
            true,
        ) {
            Ok(()) => {
                if form.action == "create" {
                    save_onboard_options(&state, effective_name, &models, auto_merge, llm_review)?;
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
            Err(err @ (StoreError::DuplicateName(_) | StoreError::DuplicateRepo(..))) => {
                report.errors.push(err.to_string());
            }
            Err(StoreError::InstanceChanged(_)) => report.errors.push(STALE_ONBOARD.to_owned()),
            Err(err) => return Err(err.into()),
        }
    }

    let view = pages::OnboardView {
        forges: state.forges()?,
        forge: forge.clone(),
        name,
        repo,
        base: base_field,
        checks,
        models: &models,
        auto_merge,
        llm_review,
        token_page: token_page.as_deref(),
    };
    Ok(pages::onboard(&user(&headers), &view, Some(&report), None).into_response())
}

fn save_onboard_options(
    state: &AppState,
    name: &str,
    models: &StageModels,
    auto_merge: bool,
    llm_review: bool,
) -> Result<(), AppError> {
    if *models != StageModels::default() {
        state.store.save_project_models(name, models)?;
    }
    if auto_merge || llm_review {
        state
            .store
            .save_project_settings(name, auto_merge, llm_review)?;
    }
    Ok(())
}

/// Deletes the jobs a removed project of this name left behind, or adds a
/// blocking error when one still has a task.
async fn clear_leftover_jobs(
    state: &AppState,
    name: &str,
    report: &mut RepoReport,
) -> Result<(), AppError> {
    let _pass = state.slots.lock_pass().await;
    let leftover = state.store.job_ids_for(name)?;
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
    Ok(())
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

enum Onboarder {
    Account(Account),
    Blocked(String),
}

fn onboarder_account(
    state: &AppState,
    headers: &HeaderMap,
    forge: &ForgeKind,
) -> Result<Onboarder, AppError> {
    let login = user(headers);
    let owner = ForgeTokenOwner::User {
        login: &login,
        forge,
    };
    let name = forge.display_name();
    match state.secrets.forge_token_with_account(owner) {
        Ok(Some((token, found))) => Ok(Onboarder::Account(
            state.forges.forge(forge).account(token, &found),
        )),
        Ok(None) => Ok(Onboarder::Blocked(format!(
            "You have no {name} token stored, and the repository checks run as your account."
        ))),
        Err(SecretError::Unavailable) => Ok(Onboarder::Blocked(format!(
            "The secret store is unavailable, so your {name} token cannot be read and the repository checks cannot run as your account."
        ))),
        Err(err) => Err(err.into()),
    }
}

/// The repository checks on `forge` plus the name and repo uniqueness rules. A forge call
/// that could not run becomes a blocking error, so the operator sees it and can
/// retry rather than the page returning a 500.
async fn onboard_report(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    forge: &ForgeKind,
    repo: &str,
    base: &str,
) -> Result<(RepoReport, Option<String>), AppError> {
    let mut token_page = None;
    let mut report = match onboarder_account(state, headers, forge)? {
        Onboarder::Account(account) => match state
            .forges
            .checker(forge)
            .check(&account, repo, base)
            .await
        {
            Ok(report) => report,
            Err(err) => RepoReport {
                errors: vec![format!("The repository checks could not run: {err}")],
                warnings: Vec::new(),
            },
        },
        Onboarder::Blocked(message) => {
            token_page = Some(format!("/settings/forges/{}", forge.as_str()));
            RepoReport {
                errors: vec![message],
                warnings: Vec::new(),
            }
        }
    };
    if !valid_name(name) {
        report.errors.insert(
            0,
            format!(
                "The project name {name:?} is empty or has a character other than a letter, digit, dash, underscore or dot."
            ),
        );
    } else if state.store.name_in_use(name)? {
        report
            .errors
            .insert(0, format!("A project named {name:?} already exists."));
    }
    if state
        .projects
        .list()?
        .iter()
        .any(|(_, p)| p.forge == ProjectForge::Known(forge.clone()) && p.repo == repo)
    {
        report.errors.push(format!(
            "The {} repository {repo} is already onboarded.",
            forge.display_name()
        ));
    }
    Ok((report, token_page))
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
