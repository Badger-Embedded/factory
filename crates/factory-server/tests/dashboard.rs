mod support;

use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::{
    Agent, AgentResult, Ask, BranchSummary, Cli, DEFAULT_AGENT, Event, Gates, MAX_FAILED_ROUNDS,
    MAX_REVIEW_ROUNDS, PlanUsage, PlannedPr, PrDescription, Question, Session, Stage, Task, Turn,
    Wait,
};
use factory_runner::{Account, ForgeAccount, ForgeKind, GitHub};
use factory_server::{
    AgentRegistry, BranchInfo, CheckOutcome, ClaudeToken, CodexLogin, DeviceLogin,
    FACTORY_REQUESTER, ForgeTokenOwner, Forges, GitHubRepos, LocalSecrets, LoginProcess,
    LoginSpawner, MASTER_KEY_LEN, McpChecker, McpProjectSettings, McpTool, Permission,
    PlanUsageCache, ProjectForge, Projects, Protection, RepoChecker, RepoInfo, RepoReport,
    SecretStore, Settings, StageAgents, StageEfforts, StageModels, Store, StoreError, ToolList,
    USER_HEADER, UnavailableSecrets, WorkerSlots, app,
};
use http_body_util::BodyExt;
use serde_json::json;
use tokio::sync::Notify;
use tower::ServiceExt;

/// A login spawner that never runs a container, for the dashboard tests that do
/// not drive a Codex login. The device login has its own test file.
struct NoLogin;

impl LoginSpawner for NoLogin {
    fn spawn(
        &self,
        _name: String,
        _image: String,
        _codex_home: std::path::PathBuf,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Box<dyn LoginProcess>>> + Send + '_>> {
        Box::pin(async {
            Err(std::io::Error::other(
                "no login container in dashboard tests",
            ))
        })
    }
}

/// A fake GitHub the onboarding wizard checks against, so tests need no network.
/// The default passes every check; a test mutates one field to fail one.
#[derive(Clone)]
struct FakeRepos {
    repo: Option<RepoInfo>,
    branch: Option<BranchInfo>,
    ci: bool,
    /// The token each forge call received, in order.
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl FakeRepos {
    /// A repository that passes every required check and raises no warning.
    fn fit() -> Self {
        FakeRepos {
            repo: Some(RepoInfo {
                permission: Permission::Write,
                allow_squash_merge: true,
                delete_branch_on_merge: true,
            }),
            branch: Some(BranchInfo {
                protection: Protection::Protected,
            }),
            ci: true,
            seen: Arc::default(),
        }
    }
}

impl GitHubRepos for FakeRepos {
    fn repo(
        &self,
        account: &Account,
        _repo: &str,
    ) -> impl Future<Output = Result<Option<RepoInfo>, String>> + Send {
        self.seen.lock().unwrap().push(account.token.clone());
        std::future::ready(Ok(self.repo.clone()))
    }

    fn branch(
        &self,
        _account: &Account,
        _repo: &str,
        _branch: &str,
    ) -> impl Future<Output = Result<Option<BranchInfo>, String>> + Send {
        std::future::ready(Ok(self.branch.clone()))
    }

    fn has_ci(
        &self,
        _account: &Account,
        _repo: &str,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        std::future::ready(Ok(self.ci))
    }
}

struct Dashboard {
    store: Arc<Store>,
    router: Router,
    data: tempfile::TempDir,
    usage: Arc<PlanUsageCache>,
    slots: Arc<WorkerSlots>,
    checker: Arc<FakeChecker>,
}

/// A fake [`McpChecker`] the dashboard holds in place of the real HTTP client.
/// A test sets the outcome it returns and reads the URL and headers it was
/// handed, so it can assert the handler resolved the saved values.
/// One recorded check: the URL and the headers the handler resolved for it.
type Checked = (String, Vec<(String, String)>);

#[derive(Default)]
struct FakeChecker {
    outcome: Mutex<Option<CheckOutcome>>,
    seen: Mutex<Vec<Checked>>,
}

impl FakeChecker {
    fn set(&self, outcome: CheckOutcome) {
        *self.outcome.lock().unwrap() = Some(outcome);
    }

    /// The headers of the last check, or an empty list if none ran.
    fn last_headers(&self) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap()
            .last()
            .map(|(_, headers)| headers.clone())
            .unwrap_or_default()
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl McpChecker for FakeChecker {
    fn check<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'a>> {
        self.seen.lock().unwrap().push((url.to_owned(), headers));
        let outcome = self
            .outcome
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(CheckOutcome::Unreachable);
        Box::pin(async move { outcome })
    }
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    content_type: Option<String>,
    headers: header::HeaderMap,
    bytes: Vec<u8>,
    body: String,
}

impl Reply {
    fn header(&self, name: header::HeaderName) -> Option<String> {
        self.headers
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    }
}

const BOUNDARY: &str = "factory-test-boundary";

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";
/// Two more valid PNGs with distinct bytes, so an edit test can tell a kept
/// image from a new one by its contents.
const PNG_B: &[u8] = b"\x89PNG\r\n\x1a\n1111";
const PNG_C: &[u8] = b"\x89PNG\r\n\x1a\n2222";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
const GIF: &[u8] = b"GIF89a0000";
const WEBP: &[u8] = b"RIFF\x04\0\0\0WEBPVP8 ";
const ZIP: &[u8] = b"PK\x03\x04 a zip archive";
const TEXT: &[u8] = b"a log line\n";

/// The Tailscale login every dashboard request carries unless a test sends its
/// own. [`dashboard_with_secrets`] seeds it as a collaborator on every project
/// in the store, so the access checks let these requests through; a test that
/// needs a non-collaborator sends a different login.
const USER: &str = "alice@example.com";

fn dashboard() -> Dashboard {
    dashboard_for("Badger-Embedded/maestro")
}

fn dashboard_for(repo: &str) -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            repo,
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    dashboard_with(store)
}

/// A dashboard reading projects from `store`, so a project onboarded in the
/// store is visible on the next request. Its onboarding checks see a fit
/// repository.
fn dashboard_with(store: Arc<Store>) -> Dashboard {
    dashboard_with_github(store, Arc::new(FakeRepos::fit()))
}

/// As [`dashboard_with`], with the onboarding checks answered by `github`, so a
/// test can fail one check. Its secret store is a ready [`LocalSecrets`].
fn dashboard_with_github(store: Arc<Store>, github: Arc<dyn RepoChecker>) -> Dashboard {
    let secrets = Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    dashboard_with_secrets(store, github, secrets)
}

/// As [`dashboard_with_github`], with the secret store given, so a test can use
/// an [`UnavailableSecrets`] to check the master-key-missing behaviour.
fn dashboard_with_secrets(
    store: Arc<Store>,
    github: Arc<dyn RepoChecker>,
    secrets: Arc<dyn SecretStore>,
) -> Dashboard {
    let admins = [USER, STRANGER, MAESTRO_ONLY].map(str::to_owned).to_vec();
    dashboard_with_admins(store, github, secrets, admins)
}

fn dashboard_with_admins(
    store: Arc<Store>,
    github: Arc<dyn RepoChecker>,
    secrets: Arc<dyn SecretStore>,
    admins: Vec<String>,
) -> Dashboard {
    dashboard_with_gh(store, github, secrets, admins, |_| GitHub::default())
}

fn dashboard_with_gh(
    store: Arc<Store>,
    github: Arc<dyn RepoChecker>,
    secrets: Arc<dyn SecretStore>,
    admins: Vec<String>,
    gh: impl FnOnce(&Path) -> GitHub,
) -> Dashboard {
    dashboard_build(
        store,
        github,
        None,
        |_| None,
        secrets,
        admins,
        gh,
        Vec::new(),
    )
}

/// As [`dashboard_with_gh`], with the Salsa onboarding checks answered by
/// `salsa` when given.
#[allow(
    clippy::too_many_arguments,
    reason = "each argument is one fake the dashboard is built from"
)]
fn dashboard_build(
    store: Arc<Store>,
    github: Arc<dyn RepoChecker>,
    salsa: Option<Arc<dyn RepoChecker>>,
    salsa_forge: impl FnOnce(&Path) -> Option<Arc<dyn factory_runner::Forge>>,
    secrets: Arc<dyn SecretStore>,
    admins: Vec<String>,
    gh: impl FnOnce(&Path) -> GitHub,
    forgejo_roots: Vec<reqwest::Certificate>,
) -> Dashboard {
    for (name, _) in store.list_projects().unwrap() {
        store.add_collaborator(&name, USER, "test").unwrap();
    }
    let projects = Arc::new(Projects::from_store(Arc::clone(&store)));
    let data = tempfile::tempdir().unwrap();
    let usage = Arc::new(PlanUsageCache::new());
    let slots = Arc::new(WorkerSlots::new());
    let checker = Arc::new(FakeChecker::default());
    let registry = Arc::new(AgentRegistry::new(
        Arc::new(ClaudeToken::new(Arc::clone(&secrets), None)),
        Arc::new(CodexLogin::new(Arc::clone(&secrets))),
        Arc::clone(&secrets),
    ));
    let device_login = Arc::new(DeviceLogin::new(
        Arc::clone(&secrets),
        Arc::new(NoLogin),
        std::env::temp_dir(),
    ));
    let copilot_login = Arc::new(DeviceLogin::for_spec(
        factory_server::LoginSpec::COPILOT,
        Arc::clone(&secrets),
        Arc::new(NoLogin),
        std::env::temp_dir(),
    ));
    Dashboard {
        router: app(
            store.clone(),
            projects,
            Arc::new(Notify::new()),
            data.path().to_owned(),
            Arc::clone(&usage),
            {
                let forges = Forges::new(Arc::new(gh(data.path())), github)
                    .with_forgejo_roots(forgejo_roots)
                    .unwrap();
                let forges = match salsa_forge(data.path()) {
                    Some(forge) => forges.with_salsa(forge),
                    None => forges,
                };
                match salsa {
                    Some(salsa) => forges.with_salsa_checker(salsa),
                    None => forges,
                }
            },
            Arc::clone(&slots),
            secrets,
            Arc::clone(&checker) as Arc<dyn McpChecker>,
            registry,
            device_login,
            copilot_login,
            admins,
        ),
        store,
        data,
        usage,
        slots,
        checker,
    }
}

impl Dashboard {
    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|value| value.to_str().unwrap().to_owned());
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned());
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            location,
            content_type,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.get_as(USER, uri).await
    }

    /// A GET sent as `login`, for the access checks that read the collaborator
    /// set; the plain `get` sends [`USER`].
    async fn get_as(&self, login: &str, uri: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(USER_HEADER, login)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn get_with_cookie(&self, login: &str, uri: &str, cookie: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(USER_HEADER, login)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// Every settings page joined into one body: the factory pages, the MCP
    /// servers page, each server's page and each project's page.
    async fn settings_pages(&self) -> String {
        let mut uris: Vec<String> = [
            "/settings/workers",
            "/settings/gates",
            "/settings/polling",
            "/settings/agents/claude",
            "/settings/agents/codex",
            "/settings/mcp",
        ]
        .map(str::to_owned)
        .into();
        let servers = self.store.mcp_servers().unwrap();
        for server in &servers {
            uris.push(format!("/settings/mcp/{}", server.name));
        }
        for (name, _) in self.store.list_projects().unwrap() {
            uris.push(format!("/settings/projects/{name}"));
            for server in &servers {
                uris.push(format!("/settings/projects/{name}/mcp/{}", server.name));
            }
        }
        let mut body = String::new();
        for uri in uris {
            let reply = self.get(&uri).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            body.push_str(&reply.body);
        }
        body
    }

    async fn post(&self, uri: &str, form: &str) -> Reply {
        self.post_as(USER, uri, form).await
    }

    /// A form POST sent as `login`, for the access checks that read the
    /// collaborator set; the plain `post` sends [`USER`].
    async fn post_as(&self, login: &str, uri: &str, form: &str) -> Reply {
        self.send(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(USER_HEADER, login)
                .body(Body::from(form.to_owned()))
                .unwrap(),
        )
        .await
    }

    async fn post_job(&self, project: &str, request: &str, images: &[(&str, &[u8])]) -> Reply {
        self.post_job_as(USER, project, request, images).await
    }

    async fn post_job_as(
        &self,
        login: &str,
        project: &str,
        request: &str,
        images: &[(&str, &[u8])],
    ) -> Reply {
        let mut body = Vec::new();
        let mut part = |disposition: String, content: &[u8]| {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n{disposition}\r\n\r\n").as_bytes());
            body.extend_from_slice(content);
            body.extend_from_slice(b"\r\n");
        };
        part(
            "Content-Disposition: form-data; name=\"project\"".into(),
            project.as_bytes(),
        );
        part(
            "Content-Disposition: form-data; name=\"request\"".into(),
            request.as_bytes(),
        );
        for (file_name, bytes) in images {
            part(
                format!(
                    "Content-Disposition: form-data; name=\"images\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream"
                ),
                bytes,
            );
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        self.send(
            Request::post("/jobs")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .header(USER_HEADER, login)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    /// Posts a request that waits for the jobs in `after`, one repeated `after`
    /// part each, as the board's checkboxes post them.
    async fn post_job_after(&self, project: &str, request: &str, after: &[&str]) -> Reply {
        let mut body = String::new();
        for (name, value) in [("project", project), ("request", request)]
            .into_iter()
            .chain(after.iter().map(|id| ("after", *id)))
        {
            write!(
                body,
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .unwrap();
        }
        write!(body, "--{BOUNDARY}--\r\n").unwrap();
        self.send(
            Request::post("/jobs")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .header(USER_HEADER, USER)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    /// Posts an edit to `/jobs/{id}/edit` as multipart: the new request text,
    /// one `remove` part per stored name to drop, and one `images` part per new
    /// upload, the way the edit form posts.
    async fn post_edit(
        &self,
        id: i64,
        request: &str,
        removes: &[&str],
        images: &[(&str, &[u8])],
    ) -> Reply {
        self.post_edit_waiting(id, request, removes, images, &[], &[])
            .await
    }

    /// [`Dashboard::post_edit`] that also posts one `after` part per id in
    /// `after` and one `keep` part per row id in `keep`.
    async fn post_edit_waiting(
        &self,
        id: i64,
        request: &str,
        removes: &[&str],
        images: &[(&str, &[u8])],
        after: &[&str],
        keep: &[&str],
    ) -> Reply {
        let mut body = Vec::new();
        let mut part = |disposition: String, content: &[u8]| {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n{disposition}\r\n\r\n").as_bytes());
            body.extend_from_slice(content);
            body.extend_from_slice(b"\r\n");
        };
        part(
            "Content-Disposition: form-data; name=\"request\"".into(),
            request.as_bytes(),
        );
        for name in removes {
            part(
                "Content-Disposition: form-data; name=\"remove\"".into(),
                name.as_bytes(),
            );
        }
        for (name, ids) in [("after", after), ("keep", keep)] {
            for value in ids {
                part(
                    format!("Content-Disposition: form-data; name=\"{name}\""),
                    value.as_bytes(),
                );
            }
        }
        for (file_name, bytes) in images {
            part(
                format!(
                    "Content-Disposition: form-data; name=\"images\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream"
                ),
                bytes,
            );
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        self.send(
            Request::post(format!("/jobs/{id}/edit"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .header(USER_HEADER, USER)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    async fn new_job(&self) -> i64 {
        let reply = self.post_job("maestro", "Add a reboot button", &[]).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        let location = reply.location.unwrap();
        location.strip_prefix("/jobs/").unwrap().parse().unwrap()
    }

    fn apply(&self, id: i64, event: Event) {
        self.store.apply(id, event).unwrap();
    }

    fn turn(&self, id: i64) -> Turn {
        self.store.get(id).unwrap().job.turn().clone()
    }
}

fn to_spec_approval(dashboard: &Dashboard, id: i64) {
    dashboard.apply(id, Event::Start);
    dashboard.apply(id, Event::Agent(AgentResult::Clarified));
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Spec("Goal: a reboot button.".into())),
    );
}

#[tokio::test]
async fn a_request_becomes_a_job_owned_by_the_tailscale_user() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Add a reboot button"));
    assert!(page.body.contains("requested by alice@example.com"));
    assert!(page.body.contains("requested"));

    let row = dashboard.store.get(id).unwrap();
    assert_eq!(row.project, "maestro");
    assert_eq!(row.job.stage(), &Stage::Requested);
}

#[tokio::test]
async fn without_the_tailscale_header_the_user_is_local() {
    let dashboard = dashboard();
    dashboard
        .store
        .add_collaborator("maestro", "local", "test")
        .unwrap();
    let reply = dashboard
        .send(
            Request::post("/jobs")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"project\"\r\n\r\nmaestro\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\nx\r\n--{BOUNDARY}--\r\n"
                )))
                .unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(dashboard.store.list().unwrap()[0].requested_by, "local");
}

#[tokio::test]
async fn bad_requests_create_no_job() {
    let dashboard = dashboard();
    let unknown = dashboard.post_job("nope", "x", &[]).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let empty = dashboard.post_job("maestro", "  ", &[]).await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
}

fn job_id_of(reply: &Reply) -> i64 {
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    reply
        .location
        .as_deref()
        .unwrap()
        .strip_prefix("/jobs/")
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn a_request_waits_for_the_jobs_it_is_posted_after() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    let reply = dashboard
        .post_job_after(
            "maestro",
            "Needs both",
            &[&first.to_string(), &second.to_string(), &first.to_string()],
        )
        .await;
    let id = job_id_of(&reply);

    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Requested
    );
    assert_eq!(
        dashboard.store.waits().unwrap()[&id],
        Wait::Waiting(vec![first, second])
    );
    let board = dashboard.get("/fragments/board?project=maestro").await.body;
    let row = board
        .split("<li ")
        .find(|row| row.contains("Needs both"))
        .unwrap();
    assert!(
        row.contains("<span class=\"badge factory\">waiting</span>"),
        "{row}"
    );
    assert!(
        row.contains(&format!(
            "waits for <a href=\"/jobs/{first}\">#{first}</a>, <a href=\"/jobs/{second}\">#{second}</a>"
        )),
        "{row}"
    );
    assert_eq!(lanes_of(&board, id), ["inbox-running"]);
}

#[tokio::test]
async fn a_deleted_prerequisite_is_plain_text_on_the_board_and_a_reused_id_is_not_linked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with(Arc::clone(&store));
    let waiting = dashboard.new_job().await;
    let gone = dashboard.new_job().await;
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO job_prerequisites (job_id, prerequisite) VALUES (?1, ?2)",
        rusqlite::params![waiting, gone],
    )
    .unwrap();
    store.delete_job(gone, Path::new("/nonexistent")).unwrap();
    let reused = store.create("maestro", "alice", "reuses the id").unwrap();
    assert_eq!(reused, gone, "SQLite reuses the highest id");

    let board = dashboard.get("/fragments/board?project=maestro").await.body;
    let row = board
        .split("<li ")
        .find(|row| row.contains(&format!("href=\"/jobs/{waiting}\"")))
        .unwrap();
    assert!(
        row.contains(&format!("waits for #{gone} (deleted before it merged)")),
        "{row}"
    );
    assert!(!row.contains(&format!("href=\"/jobs/{gone}\"")), "{row}");
}

#[tokio::test]
async fn a_request_with_a_prerequisite_that_cannot_be_waited_for_is_refused() {
    let dashboard = dashboard();
    let store = &dashboard.store;
    let ok = dashboard.new_job().await;
    let other = store.create("elsewhere", "bob", "other").unwrap();
    let renovate = store
        .create_renovate_job("maestro", 7, "Update serde", "https://example/pull/7")
        .unwrap()
        .unwrap();
    let proposed = store
        .create_gap_job("maestro", renovate, 7, "Add a flag")
        .unwrap();
    let before = store.list().unwrap().len();

    let unknown = dashboard.post_job_after("maestro", "x", &["999"]).await;
    let foreign = dashboard
        .post_job_after("maestro", "x", &[&other.to_string()])
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(foreign.status, StatusCode::BAD_REQUEST);
    assert!(
        unknown
            .body
            .contains("<p>job #999 is not a job this request can wait for</p>")
    );
    assert!(foreign.body.contains(&format!(
        "<p>job #{other} is not a job this request can wait for</p>"
    )));
    for refused in [
        renovate.to_string(),
        proposed.to_string(),
        "soon".to_owned(),
    ] {
        let reply = dashboard
            .post_job_after("maestro", "x", &[&ok.to_string(), &refused])
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{refused}");
    }
    assert_eq!(store.list().unwrap().len(), before);
}

#[tokio::test]
async fn a_merged_prerequisite_does_not_hold_the_request() {
    let dashboard = dashboard();
    let done = merged(&dashboard).await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "After a merge", &[&done.to_string()])
            .await,
    );
    assert!(dashboard.store.waits().unwrap().is_empty());
    let board = dashboard.get("/fragments/board?project=maestro").await.body;
    let row = board
        .split("<li ")
        .find(|row| row.contains("After a merge"))
        .unwrap();
    assert!(row.contains(">queued</span>"), "{row}");
    assert!(!row.contains("waits for"), "{row}");
    assert!(dashboard.store.start(id).unwrap().is_some());
}

#[tokio::test]
async fn a_request_blocked_by_its_prerequisite_needs_you() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Blocked one", &[&first.to_string()])
            .await,
    );
    dashboard.apply(first, Event::Cancel);

    let board = dashboard.get("/fragments/board?project=maestro").await.body;
    assert_eq!(lanes_of(&board, id), ["inbox-you"]);
    let row = board
        .split("<li ")
        .find(|row| row.contains("Blocked one"))
        .unwrap();
    assert!(
        row.contains("<span class=\"badge you\">blocked</span>"),
        "{row}"
    );
    assert!(
        row.contains(&format!(
            "waits for <a href=\"/jobs/{first}\">#{first}</a> (cancelled)"
        )),
        "{row}"
    );
    assert!(
        row.contains(&format!(
            "<a class=\"link-button\" href=\"/jobs/{id}\">Open</a>"
        )),
        "{row}"
    );

    let notes = notifications_as(&dashboard, USER).await;
    let kinds: Vec<_> = notes["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["job"] == id)
        .map(|item| item["kind"].clone())
        .collect();
    assert_eq!(kinds, ["needs-you"]);
}

fn status_card_of(body: &str) -> &str {
    let start = body.find("id=\"job-status\"").unwrap();
    let rest = &body[start..];
    &rest[..rest.find("id=\"job-spec\"").unwrap_or(rest.len())]
}

#[tokio::test]
async fn the_status_card_lists_what_a_waiting_job_waits_for() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    let id = job_id_of(
        &dashboard
            .post_job_after(
                "maestro",
                "Needs both",
                &[&first.to_string(), &second.to_string()],
            )
            .await,
    );
    dashboard.apply(first, Event::Start);

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    let card = status_card_of(&page);
    assert!(card.contains(">waiting</span>"), "{card}");
    assert!(
        card.contains(&format!("<a href=\"/jobs/{first}\">#{first}</a>")),
        "{card}"
    );
    assert!(
        card.contains("implementing") || card.contains("clarifying"),
        "{card}"
    );
    assert!(card.contains(&format!("#{second}</a>")), "{card}");
    assert!(card.contains("requested"), "{card}");
    assert!(!card.contains("Waiting for a free worker."), "{card}");
    assert!(!card.contains("will not start"), "{card}");

    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await.body;
    assert!(fragment.contains(">waiting</span>"), "{fragment}");
    assert!(fragment.contains(&format!("#{second}</a>")), "{fragment}");

    dashboard.apply(second, Event::Cancel);
    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    let card = status_card_of(&body);
    assert!(
        card.contains(&format!(
            "will not start until #{second} merges or is removed from the list"
        )),
        "{card}"
    );
    assert!(card.contains(&format!("#{first}</a>")), "{card}");
}

#[tokio::test]
async fn a_blocked_job_says_what_unblocks_it_and_links_to_the_edit_page() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Blocked one", &[&first.to_string()])
            .await,
    );
    let waiting = dashboard.get(&format!("/jobs/{id}/fragment")).await.body;
    assert!(!waiting.contains("will not start"), "{waiting}");

    dashboard.apply(first, Event::Cancel);
    for uri in [format!("/jobs/{id}"), format!("/jobs/{id}/fragment")] {
        let body = dashboard.get(&uri).await.body;
        let card = status_card_of(&body);
        assert!(
            card.contains("<span class=\"badge you\">blocked</span>"),
            "{card}"
        );
        assert!(
            card.contains(&format!(
                "will not start until #{first} merges or is removed from the list"
            )),
            "{card}"
        );
        assert!(card.contains("cancelled"), "{card}");
        assert!(
            card.contains(&format!("href=\"/jobs/{id}/edit\"")),
            "{card}"
        );
        assert!(!card.contains("Waiting for a free worker."), "{card}");
    }
}

#[tokio::test]
async fn a_deleted_prerequisite_shows_as_deleted_before_it_merged() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Orphan", &[&first.to_string()])
            .await,
    );
    dashboard
        .store
        .delete_job(first, Path::new("/nonexistent"))
        .unwrap();

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    let card = status_card_of(&body);
    assert!(card.contains("deleted before it merged"), "{card}");
    assert!(card.contains("Add a reboot button"), "{card}");
    assert!(!card.contains(&format!("href=\"/jobs/{first}\"")), "{card}");
    assert!(card.contains("will not start"), "{card}");
}

#[tokio::test]
async fn a_started_job_still_shows_the_jobs_it_was_queued_after() {
    let dashboard = dashboard();
    let done = merged(&dashboard).await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "After a merge", &[&done.to_string()])
            .await,
    );
    let plain = dashboard.new_job().await;
    assert!(
        !dashboard
            .get(&format!("/jobs/{plain}"))
            .await
            .body
            .contains("Queued after")
    );

    dashboard.apply(id, Event::Start);
    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(
        body.contains(&format!(
            "Queued after <a href=\"/jobs/{done}\">#{done}</a>"
        )),
        "{body}"
    );
    assert!(!status_card_of(&body).contains("will not start"));
    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await.body;
    assert!(fragment.contains("Queued after"), "{fragment}");
}

#[tokio::test]
async fn the_composer_offers_the_projects_unfinished_jobs_to_start_after() {
    let dashboard = dashboard();
    let store = &dashboard.store;
    let none = dashboard.get("/").await.body;
    assert!(!none.contains("Start after"));

    let open = dashboard.new_job().await;
    let done = merged(&dashboard).await;
    let cancelled = dashboard.new_job().await;
    dashboard.apply(cancelled, Event::Cancel);
    let stuck = dashboard.new_job().await;
    apply_all(
        store,
        stuck,
        [
            Event::Start,
            Event::AgentFailed {
                reason: "usage limit".into(),
            },
        ],
    );
    let dead = dashboard.new_job().await;
    dashboard.apply(dead, Event::Cancel);
    store.create("elsewhere", "bob", "other").unwrap();
    let renovate = store
        .create_renovate_job("maestro", 7, "Update serde", "https://example/pull/7")
        .unwrap()
        .unwrap();
    let proposed = store
        .create_gap_job("maestro", renovate, 7, "Add a flag")
        .unwrap();

    let page = dashboard.get("/").await.body;
    assert!(page.contains("<legend>Start after</legend>"));
    let offered = |id: i64| {
        page.contains(&format!(
            "<input type=\"checkbox\" name=\"after\" value=\"{id}\">"
        ))
    };
    assert!(offered(open));
    assert!(offered(stuck));
    for hidden in [done, cancelled, dead, renovate, proposed] {
        assert!(!offered(hidden), "job {hidden}");
    }
}

fn apply_all(store: &Store, id: i64, events: impl IntoIterator<Item = Event>) {
    for event in events {
        store.apply(id, event).unwrap();
    }
}

#[tokio::test]
async fn the_index_lists_jobs_and_flags_those_waiting_for_the_user() {
    let dashboard = dashboard();
    let page = dashboard.get("/").await;
    assert_current_project(&page.body, "maestro", "maestro");
    assert!(page.body.contains("name=\"project\" value=\"maestro\""));

    let id = dashboard.new_job().await;
    let page = dashboard.get("/").await;
    assert!(page.body.contains(&format!("href=\"/jobs/{id}\"")));
    assert!(!page.body.contains("class=\"inbox inbox-you\""));

    to_spec_approval(&dashboard, id);
    let page = dashboard.get("/").await;
    assert!(page.body.contains("class=\"inbox inbox-you\""));
    assert!(page.body.contains("class=\"badge you\""));
}

#[tokio::test]
async fn the_stylesheet_is_served_as_css() {
    let dashboard = dashboard();
    assert!(
        dashboard
            .get("/")
            .await
            .body
            .contains("href=\"/style.css\"")
    );

    let response = dashboard
        .router
        .clone()
        .oneshot(Request::get("/style.css").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/css; charset=utf-8"
    );
}

#[tokio::test]
async fn htmx_is_served_from_the_factory_as_javascript() {
    let dashboard = dashboard();
    assert!(
        dashboard
            .get("/")
            .await
            .body
            .contains("src=\"/htmx.min.js\"")
    );

    let reply = dashboard.get("/htmx.min.js").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .content_type
            .as_deref()
            .is_some_and(|value| value.contains("javascript")),
        "content type was {:?}",
        reply.content_type
    );
    assert!(reply.body.contains("htmx"));
}

#[tokio::test]
async fn the_dashboard_loads_no_asset_from_an_outside_host() {
    // The job page links to GitHub with an `<a>`, which the reader clicks. An
    // outside host matters only for assets the page fetches on load: scripts
    // and stylesheets.
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    for uri in [
        "/",
        "/settings/workers",
        "/settings/mcp",
        "/settings/projects/maestro",
        &format!("/jobs/{id}"),
    ] {
        let body = dashboard.get(uri).await.body;
        for attr in ["src=\"http", "src=\"//", "rel=\"stylesheet\" href=\"http"] {
            assert!(!body.contains(attr), "{uri} loads an outside asset: {attr}");
        }
    }
}

#[tokio::test]
async fn answers_from_the_form_go_back_to_the_agent_in_order() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    let questions = vec![
        Question {
            text: "Which board?".into(),
        },
        Question {
            text: "Ask before rebooting?".into(),
        },
    ];
    dashboard.apply(id, Event::Agent(AgentResult::Questions(questions.clone())));

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Which board?"));
    assert!(page.body.contains("name=\"answer_1\""));
    assert!(
        page.body
            .contains(r#"<span class="question-number">2</span>"#)
    );
    assert!(
        page.body
            .contains(r#"<span class="badge you">needs you</span>"#)
    );
    assert!(page.body.contains("card needs-you"));

    let reply = dashboard
        .post(
            &format!("/jobs/{id}/answers"),
            "answer_1=yes&answer_0=+rpi5+",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.turn(id),
        Turn::Agent(Task::Answer {
            questions,
            answers: vec!["rpi5".into(), "yes".into()],
        })
    );
}

#[tokio::test]
async fn too_few_answers_are_rejected() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    let questions = vec![
        Question { text: "a?".into() },
        Question { text: "b?".into() },
    ];
    dashboard.apply(id, Event::Agent(AgentResult::Questions(questions.clone())));

    let reply = dashboard
        .post(&format!("/jobs/{id}/answers"), "answer_0=only")
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(dashboard.turn(id), Turn::User(Ask::Answers { questions }));
}

#[tokio::test]
async fn the_spec_can_be_sent_back_and_then_approved() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Goal: a reboot button."));
    assert!(page.body.contains("Approve spec"));

    let reply = dashboard
        .post(&format!("/jobs/{id}/changes"), "feedback=Drop+the+CLI")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.turn(id),
        Turn::Agent(Task::Revise {
            feedback: "Drop the CLI".into()
        })
    );

    dashboard.apply(id, Event::Agent(AgentResult::Spec("v2".into())));
    dashboard.post(&format!("/jobs/{id}/approve"), "").await;
    assert_eq!(dashboard.store.get(id).unwrap().job.stage(), &Stage::Plan);
}

#[tokio::test]
async fn the_plan_page_lists_the_planned_pull_requests() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![PlannedPr {
            title: "maestrod: add a reboot endpoint".into(),
            summary: "POST /reboot".into(),
            number: None,
            description: None,
            notes: None,
        }])),
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("maestrod: add a reboot endpoint"));
    assert!(page.body.contains("Approve plan"));

    dashboard.post(&format!("/jobs/{id}/approve"), "").await;
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Implementing { pr: 0 }
    );
}

#[tokio::test]
async fn an_action_the_job_does_not_expect_is_a_conflict() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let reply = dashboard.post(&format!("/jobs/{id}/approve"), "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Requested
    );
}

#[tokio::test]
async fn a_cancelled_job_offers_no_cancel_button() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    assert!(
        dashboard
            .get(&format!("/jobs/{id}"))
            .await
            .body
            .contains("Cancel job")
    );

    dashboard.post(&format!("/jobs/{id}/cancel"), "").await;
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("cancelled"));
    assert!(!page.body.contains("Cancel job"));
}

#[tokio::test]
async fn the_job_page_shows_the_cost_summed_over_runs() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 0.5, false).unwrap();
    dashboard.store.add_cost(id, 0.75, false).unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains("<span class=\"badge cost\" id=\"job-cost\">$1.25</span>")
    );
    assert!(page.body.contains("requested by alice@example.com"));
    // The created_at is wrapped so the browser can show it in its own zone.
    // Without JavaScript it reads the stored UTC time, labelled UTC.
    assert!(page.body.contains("data-format=\"datetime\""));
    assert!(page.body.contains("UTC</time></p>"));
}

#[tokio::test]
async fn the_cost_pill_marks_a_total_that_includes_an_estimate() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 0.5, false).unwrap();
    dashboard.store.add_cost(id, 0.25, true).unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    // The pill carries a ≈ and a title that names the Codex estimate.
    assert!(page.body.contains("≈ $0.75"));
    assert!(page.body.contains("Includes an estimate for Codex runs."));
}

/// The job header wraps a known `created_at` in a `<time>` element whose
/// `datetime` is the stored UTC time in ISO 8601, so `local-time.js` can show
/// it in the browser's zone, and whose text is that UTC time for a visitor
/// with JavaScript off.
#[tokio::test]
async fn the_job_header_wraps_created_at_in_a_time_element() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with(Arc::clone(&store));
    let id = dashboard.new_job().await;
    set_created_at(&path, id, "2026-11-20 09:15:00");

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains(
        "<time datetime=\"2026-11-20T09:15:00Z\" data-format=\"datetime\">2026-11-20 09:15:00 UTC</time>"
    ));
}

/// Sets a job's stored `created_at` through a second connection, the way the
/// migration tests write rows the dashboard then reads.
fn set_created_at(path: &Path, id: i64, at: &str) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute(
            "UPDATE jobs SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![at, id],
        )
        .unwrap();
}

#[tokio::test]
async fn the_timeline_notes_an_unavailable_mcp_server_under_its_step() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    // Start moves the job to clarifying, the step the warning should sit under.
    dashboard.apply(id, Event::Start);
    dashboard
        .store
        .record_mcp_unavailable(id, "context7", "the server could not be reached")
        .unwrap();

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    let note = body
        .find("MCP server unavailable: context7")
        .expect("the note renders");
    let clarifying = body
        .find("step-name\">clarifying")
        .expect("the clarifying step");
    let spec = body.find("step-name\">spec").expect("the spec step");
    assert!(
        clarifying < note && note < spec,
        "the note sits under the clarifying step"
    );
}

#[tokio::test]
async fn the_timeline_notes_a_refused_tool_call_under_its_step() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard
        .store
        .record_mcp_tool_blocked(id, "context7", "delete")
        .unwrap();

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    let note = body
        .find("MCP tool turned off, call refused: context7: delete")
        .expect("the note renders");
    let clarifying = body
        .find("step-name\">clarifying")
        .expect("the clarifying step");
    let spec = body.find("step-name\">spec").expect("the spec step");
    assert!(
        clarifying < note && note < spec,
        "the note sits under the clarifying step"
    );
}

#[tokio::test]
async fn mcp_tool_call_rows_do_not_change_the_timeline() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard.store.add_cost(id, 1.5, false).unwrap();

    // The timeline region, from its id to the end of the side column.
    let timeline = |body: &str| {
        let start = body
            .find("id=\"job-timeline\"")
            .expect("the timeline region");
        let end = body[start..].find("</aside>").expect("the region ends") + start;
        body[start..end].to_owned()
    };

    let before = timeline(&dashboard.get(&format!("/jobs/{id}")).await.body);

    // Forwarded MCP calls are recorded as rows. They add no note and store the
    // stage the job is already in, so the timeline's steps, notes, durations and
    // costs are unchanged.
    dashboard
        .store
        .record_mcp_tool_call(id, "context7", "resolve-library-id")
        .unwrap();
    dashboard
        .store
        .record_mcp_tool_call(id, "context7", "get-library-docs")
        .unwrap();

    let after = timeline(&dashboard.get(&format!("/jobs/{id}")).await.body);
    assert_eq!(before, after);
}

#[tokio::test]
async fn a_job_with_no_mcp_calls_has_no_mcp_card() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(!body.contains("MCP tool calls"));
    // The out-of-band wrapper is still there, empty, so a poll can fill it.
    assert!(body.contains("<div id=\"job-mcp-calls\"></div>"));
}

#[tokio::test]
async fn the_job_page_groups_mcp_tool_calls_oldest_first() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard
        .store
        .record_mcp_tool_call(id, "context7", "resolve-library-id")
        .unwrap();
    dashboard
        .store
        .record_mcp_tool_call(id, "context7", "get-library-docs")
        .unwrap();
    dashboard
        .store
        .record_mcp_tool_blocked(id, "context7", "delete")
        .unwrap();

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(body.contains("MCP tool calls"));
    // The header counts every call across servers and the refused ones.
    assert!(body.contains("3 calls"));
    assert!(body.contains("1 refused"));

    // The three calls are one collapsed group for the one server, and it
    // carries a stable key so `details-state.js` can reopen it after a swap.
    assert!(body.contains("data-server=\"context7\""));
    assert!(body.contains(
        "<details class=\"mcp-call-group\" data-server=\"context7\" data-key=\"context7\">"
    ));
    assert_eq!(body.matches("<details").count(), 1);
    assert!(!body.contains("data-key=\"context7\" open"));

    // The expanded rows hold the tools in recording order, with the server
    // dropped since the summary shows it.
    let first = body.find("resolve-library-id").expect("the first call");
    let second = body.find("get-library-docs").expect("the second call");
    let third = body.find(">delete<").expect("the refused call");
    assert!(first < second && second < third, "rows are oldest first");
    assert!(!body.contains("context7 · delete"));

    // The stage the job was in, from the timeline's labels.
    assert!(body.contains("clarifying"));

    // A forwarded call is neutral and a refused one is bad.
    assert_eq!(body.matches("class=\"badge neutral\">called").count(), 2);
    assert!(body.contains("class=\"badge bad\">refused: tool turned off"));
    // The summary carries a counter badge and a bad refused badge.
    assert!(body.contains("class=\"badge neutral\">3"));
    assert!(body.contains("class=\"badge bad\">1 refused"));
}

#[tokio::test]
async fn one_server_many_calls_groups_and_one_call_stays_a_plain_row() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    for tool in ["a", "b", "c"] {
        dashboard
            .store
            .record_mcp_tool_call(id, "context7", tool)
            .unwrap();
    }
    dashboard
        .store
        .record_mcp_tool_call(id, "other", "lone")
        .unwrap();

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;

    // The three-call server is one collapsed group with a counter badge of 3.
    assert_eq!(body.matches("<details").count(), 1);
    assert!(body.contains("data-server=\"context7\""));
    assert!(body.contains("class=\"badge neutral\">3"));

    // The one-call server is a plain row with no details and no counter badge,
    // and its tool is visible next to the server.
    assert!(body.contains("other · lone"));
    assert!(!body.contains("data-server=\"other\""));
}

#[tokio::test]
async fn mcp_groups_are_ordered_by_each_servers_first_call() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    // A, B, A: server A's first call comes before B's, so A's entry leads even
    // though A also has the last call. A is a two-call group, B a plain row.
    dashboard.store.record_mcp_tool_call(id, "a", "a1").unwrap();
    dashboard.store.record_mcp_tool_call(id, "b", "b1").unwrap();
    dashboard.store.record_mcp_tool_call(id, "a", "a2").unwrap();

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    let a = body.find("data-server=\"a\"").expect("server a group");
    let b = body.find("b · b1").expect("server b row");
    assert!(a < b, "server a's entry comes before server b's");
}

#[tokio::test]
async fn an_mcp_group_counter_badge_appears_only_at_two_calls() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard
        .store
        .record_mcp_tool_call(id, "solo", "one")
        .unwrap();

    // One call: a plain row, no group and no counter badge.
    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(!body.contains("<details"));
    assert!(!body.contains("class=\"badge neutral\">1<"));

    // A second call on the same server turns the row into a group with a badge
    // reading 2.
    dashboard
        .store
        .record_mcp_tool_call(id, "solo", "two")
        .unwrap();
    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert_eq!(body.matches("<details").count(), 1);
    assert!(body.contains("data-server=\"solo\""));
    assert!(body.contains("class=\"badge neutral\">2"));
}

#[tokio::test]
async fn an_mcp_tool_name_is_escaped() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard
        .store
        .record_mcp_tool_call(id, "context7", "<script>alert(1)</script>")
        .unwrap();

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!body.contains("<script>alert(1)</script>"));
}

#[tokio::test]
async fn an_mcp_server_name_is_escaped_in_a_group_summary() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    // Two calls so the server renders a group summary and a data-server value.
    for tool in ["one", "two"] {
        dashboard
            .store
            .record_mcp_tool_call(id, "<script>alert(1)</script>", tool)
            .unwrap();
    }

    let body = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!body.contains("<script>alert(1)</script>"));
}

#[tokio::test]
async fn the_job_fragment_swaps_the_mcp_calls() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard
        .store
        .record_mcp_tool_call(id, "context7", "resolve-library-id")
        .unwrap();

    // A poll updates the card out of band, so a call shows without a reload.
    let reply = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .body
            .contains("<div id=\"job-mcp-calls\" hx-swap-oob=\"true\">")
    );
    assert!(reply.body.contains("resolve-library-id"));
}

#[tokio::test]
async fn a_job_without_cost_shows_a_dash() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains("<span class=\"badge cost\" id=\"job-cost\">–</span>")
    );
}

#[tokio::test]
async fn a_zero_cost_of_a_subscription_run_still_shows_a_dash() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 0.0, false).unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains("<span class=\"badge cost\" id=\"job-cost\">–</span>")
    );
    let index = dashboard.get("/").await;
    assert!(index.body.contains("<span class=\"cost\">–</span>"));
}

#[tokio::test]
async fn a_job_that_only_ran_on_local_shows_zero_dollars_with_no_estimate_mark() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 0.0, false).unwrap();
    dashboard
        .store
        .session_uuid(id, Session::Planning, Agent::Local)
        .unwrap();
    dashboard
        .store
        .record_session_local_cli(id, Session::Planning, Cli::Codex)
        .unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains("<span class=\"badge cost\" id=\"job-cost\">$0.00</span>")
    );
    let index = dashboard.get("/").await;
    assert!(index.body.contains("<span class=\"cost\">$0.00</span>"));
    assert!(!page.body.contains("≈"));
}

#[tokio::test]
async fn a_cancelled_job_still_shows_its_cost() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 2.0, false).unwrap();
    dashboard.post(&format!("/jobs/{id}/cancel"), "").await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("cancelled"));
    assert!(
        page.body
            .contains("<span class=\"badge cost\" id=\"job-cost\">$2.00</span>")
    );
}

#[tokio::test]
async fn an_unknown_job_is_not_found() {
    let dashboard = dashboard();
    assert_eq!(
        dashboard.get("/jobs/42").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dashboard.post("/jobs/42/cancel", "").await.status,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn jobs_survive_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let id = {
        let store = Store::open(&path).unwrap();
        let id = store
            .create("maestro", "alice", "Add a reboot button")
            .unwrap();
        store.apply(id, Event::Start).unwrap();
        id
    };

    let row = Store::open(&path).unwrap().get(id).unwrap();
    assert_eq!(row.job.stage(), &Stage::Clarifying);
    assert_eq!(row.requested_by, "alice");
}

#[test]
fn a_rejected_event_is_not_saved() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    let before = store.get(id).unwrap();

    assert!(matches!(
        store.apply(id, Event::Approve),
        Err(StoreError::Rejected(_))
    ));
    assert_eq!(store.get(id).unwrap(), before);
}

/// Writes a `<dir>/<name>/project.toml` for the import tests.
fn write_toml(dir: &Path, name: &str, toml: &str) {
    fs::create_dir_all(dir.join(name)).unwrap();
    fs::write(dir.join(name).join("project.toml"), toml).unwrap();
}

#[test]
fn the_import_moves_project_files_into_the_database() {
    let dir = tempfile::tempdir().unwrap();
    write_toml(
        dir.path(),
        "maestro",
        "repo = \"Badger-Embedded/maestro\"\nbase = \"main\"\n\
         checks = \"just lint && just test\"\n",
    );
    write_toml(
        dir.path(),
        "factory",
        "repo = \"Badger-Embedded/factory\"\nbase = \"dev\"\n\
         auto_merge = true\nllm_review = true\n[models]\nplan = \"opus\"\n",
    );

    let store = Store::open_in_memory().unwrap();
    factory_server::import_tomls(&store, dir.path()).unwrap();

    let maestro = store.project("maestro").unwrap().unwrap();
    assert_eq!(maestro.repo, "Badger-Embedded/maestro");
    assert_eq!(maestro.base, "main");
    assert_eq!(maestro.checks.as_deref(), Some("just lint && just test"));
    // maestro set no models or flags, so it has no settings rows and reads the
    // defaults.
    let none = StageModels::default();
    assert_eq!(store.project_models("maestro", &none).unwrap(), none);
    assert!(!store.project_auto_merge("maestro", false).unwrap());
    assert!(!store.project_llm_review("maestro", false).unwrap());

    let factory = store.project("factory").unwrap().unwrap();
    assert_eq!(factory.repo, "Badger-Embedded/factory");
    assert_eq!(factory.base, "dev");
    assert_eq!(factory.checks, None);
    assert_eq!(
        store
            .project_models("factory", &none)
            .unwrap()
            .plan
            .as_deref(),
        Some("opus")
    );
    assert!(store.project_auto_merge("factory", false).unwrap());
    assert!(store.project_llm_review("factory", false).unwrap());

    // The files it imported are gone.
    assert!(!dir.path().join("maestro").join("project.toml").exists());
    assert!(!dir.path().join("factory").join("project.toml").exists());
}

#[test]
fn the_import_runs_once_and_keeps_later_edits() {
    let dir = tempfile::tempdir().unwrap();
    write_toml(
        dir.path(),
        "maestro",
        "repo = \"o/maestro\"\nbase = \"main\"\n",
    );
    let store = Store::open_in_memory().unwrap();
    factory_server::import_tomls(&store, dir.path()).unwrap();

    // An operator edits the project's settings after the import.
    store.save_project_settings("maestro", true, true).unwrap();

    // A rerun, even with the file back on disk, changes nothing: one project
    // and the edited settings stand.
    write_toml(
        dir.path(),
        "maestro",
        "repo = \"o/other\"\nbase = \"dev\"\n",
    );
    factory_server::import_tomls(&store, dir.path()).unwrap();

    assert_eq!(store.list_projects().unwrap().len(), 1);
    let maestro = store.project("maestro").unwrap().unwrap();
    assert_eq!(maestro.repo, "o/maestro");
    assert_eq!(maestro.base, "main");
    assert!(store.project_auto_merge("maestro", false).unwrap());
    assert!(store.project_llm_review("maestro", false).unwrap());
}

#[test]
fn the_import_is_a_no_op_without_files() {
    let store = Store::open_in_memory().unwrap();
    factory_server::import_tomls(&store, Path::new("/no/such/projects/dir")).unwrap();
    assert_eq!(store.list_projects().unwrap().len(), 0);
}

#[test]
fn a_malformed_project_file_fails_the_import() {
    let dir = tempfile::tempdir().unwrap();
    write_toml(
        dir.path(),
        "x",
        "repo = \"a/b\"\nbase = \"main\"\nbranch = \"dev\"\n",
    );
    let store = Store::open_in_memory().unwrap();
    let err = factory_server::import_tomls(&store, dir.path()).unwrap_err();
    assert!(err.to_string().contains("project.toml"));
    // A failed import wrote no project.
    assert_eq!(store.list_projects().unwrap().len(), 0);
}

#[test]
fn the_import_leaves_the_global_auto_merge_migration_to_apply() {
    // A database carrying the old global auto-merge, on.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    store_legacy_settings(&path, LEGACY_SETTINGS_ON);

    let files = tempfile::tempdir().unwrap();
    write_toml(
        files.path(),
        "maestro",
        "repo = \"o/maestro\"\nbase = \"main\"\n",
    );
    factory_server::import_tomls(&store, files.path()).unwrap();

    // The import wrote no settings row for maestro, so the migration still
    // moves the old global auto-merge onto it, as it did for the toml project.
    store.migrate_global_auto_merge(&["maestro"]).unwrap();
    assert!(store.project_auto_merge("maestro", false).unwrap());
}

#[test]
fn create_project_rejects_a_duplicate_name() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/one",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    assert!(matches!(
        store.create_project("maestro", &ForgeKind::GitHub, "o/two", "main", None, Some("alice")),
        Err(StoreError::DuplicateName(name)) if name == "maestro"
    ));
    // The rejected call wrote nothing.
    assert_eq!(store.list_projects().unwrap().len(), 1);
    assert_eq!(store.project("maestro").unwrap().unwrap().repo, "o/one");
}

#[test]
fn create_project_rejects_a_duplicate_repo() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/repo",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    assert!(matches!(
        store.create_project("tool", &ForgeKind::GitHub, "o/repo", "main", None, Some("alice")),
        Err(StoreError::DuplicateRepo(_, repo)) if repo == "o/repo"
    ));
    assert_eq!(store.list_projects().unwrap().len(), 1);
}

#[tokio::test]
async fn a_project_onboarded_at_runtime_is_visible_without_a_restart() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let dashboard = dashboard_with(Arc::clone(&store));

    // No project yet: the new-job form offers none.
    assert!(!dashboard.get("/").await.body.contains("widgets"));

    // Onboard one while the dashboard runs, as the wizard will.
    store
        .create_project(
            "widgets",
            &ForgeKind::GitHub,
            "acme/widgets",
            "main",
            None,
            Some(USER),
        )
        .unwrap();

    // It shows on the board selector, the settings page and the new-job form
    // at once, and a job can be created against it.
    let index = dashboard.get("/?project=widgets").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("widgets"));
    assert!(dashboard.settings_pages().await.contains("widgets"));
    let created = dashboard.post_job("widgets", "A widget change", &[]).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_add_a_server_name_field_carries_the_name_rule() {
    // The browser blocks a bad name before submit; the attributes must mirror
    // `validate_name` in mcp.rs, which stays the server-side authority.
    let dashboard = dashboard();
    let body = dashboard.settings_pages().await;
    let name_input = body
        .split("id=\"mcp-new-name\"")
        .nth(1)
        .and_then(|rest| rest.split('>').next())
        .expect("the Add a server form has a Name input");
    assert!(name_input.contains("pattern=\"[a-z0-9\\-]{1,32}\""));
    assert!(name_input.contains("maxlength=\"32\""));
    assert!(name_input.contains("required"));
    assert!(name_input.contains("title="));
}

/// Collects every `pattern="..."` attribute value in a page, in order.
fn pattern_attributes(page: &str) -> Vec<String> {
    page.match_indices("pattern=\"")
        .filter_map(|(start, marker)| {
            let rest = &page[start + marker.len()..];
            rest.split('"').next().map(str::to_owned)
        })
        .collect()
}

/// Reports whether every `-` or `|` inside a `[...]` class is escaped, as the
/// `v` flag a browser compiles `pattern` with demands. A single `|` and a `-`
/// that cannot form a range (at a class edge) are syntax under `v`; the HTML
/// spec says to ignore an invalid pattern, so an unescaped one turns the check
/// off with no error. A range dash (`a-z`) stays literal and is left alone.
fn class_metachars_escaped(pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut in_class = false;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '[' => in_class = true,
            ']' => in_class = false,
            '|' if in_class => return false,
            '-' if in_class => {
                let at_class_edge =
                    chars.get(i - 1) == Some(&'[') || matches!(chars.get(i + 1), Some(&']') | None);
                if at_class_edge {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    true
}

#[tokio::test]
async fn settings_form_patterns_are_valid_under_the_v_flag() {
    // A browser compiles an HTML `pattern` as a RegExp with the `v` flag, where
    // an unescaped `-` or `|` inside a `[...]` class is a syntax error. An
    // invalid pattern is silently ignored, so it would stop blocking bad input
    // with no visible sign. Every pattern the Settings page renders (the server
    // name, and the header name on the add form and each server's edit form)
    // must escape those. This fails against the #96 patterns `[a-z0-9-]{1,32}`
    // and `[A-Za-z0-9!#$%&'*+.^_`|~-]+`.
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;
    let page = dashboard.settings_pages().await;
    let patterns = pattern_attributes(&page);
    // The name input and at least one header input (add form plus the edit form
    // of the server just saved) render a pattern.
    assert!(patterns.len() >= 2, "patterns rendered: {patterns:?}");
    for pattern in &patterns {
        assert!(
            class_metachars_escaped(pattern),
            "pattern leaves a `-` or `|` unescaped inside a class: {pattern}"
        );
    }
}

#[test]
fn class_metachars_escaped_catches_the_pr_96_patterns() {
    // The #96 patterns this test must reject, and the fixed forms it must pass.
    assert!(!class_metachars_escaped("[a-z0-9-]{1,32}"));
    assert!(!class_metachars_escaped("[A-Za-z0-9!#$%&'*+.^_`|~-]+"));
    assert!(class_metachars_escaped(r"[a-z0-9\-]{1,32}"));
    assert!(class_metachars_escaped(r"[A-Za-z0-9!#$%&'*+.^_`\|~\-]+"));
    // A range dash stays literal and is not flagged.
    assert!(class_metachars_escaped("[a-z0-9]+"));
}

#[test]
fn settings_default_until_saved() {
    let store = Store::open_in_memory().unwrap();
    let settings = store.settings().unwrap();
    assert_eq!(settings.max_workers, 2);
    assert_eq!(settings.worker_memory, "3g");

    let changed = factory_server::Settings {
        max_workers: 4,
        ..settings
    };
    store.save_settings(&changed).unwrap();
    assert_eq!(store.settings().unwrap(), changed);
}

/// The attribute that makes the job page poll its fragment.
fn poll_attr(id: i64) -> String {
    format!("hx-get=\"/jobs/{id}/fragment\"")
}

fn append_a_log_line(dashboard: &Dashboard, id: i64) {
    dashboard
        .store
        .append_log(
            id,
            &factory_runner::LogLine::Tool {
                name: "Bash".into(),
                summary: "cargo test".into(),
            },
        )
        .unwrap();
}

#[tokio::test]
async fn the_job_page_shows_the_log_and_polls_while_the_agent_works() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    append_a_log_line(&dashboard, id);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Bash: cargo test"));
    assert!(
        page.body
            .contains(r#"<span class="log-count">1 line</span>"#)
    );
    // No meta refresh: the page polls its fragment every five seconds instead.
    assert!(!page.body.contains("http-equiv"));
    assert!(page.body.contains(&poll_attr(id)));
    assert!(page.body.contains("hx-trigger=\"every 5s\""));
}

#[tokio::test]
async fn the_job_page_loads_its_update_script_but_the_board_does_not() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    // The job page carries the script that keeps the log scroll and a typed
    // form across a swap, with the live regions it drives.
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("sessionStorage"));
    for region in ["id=\"job-status\"", "id=\"job-log\"", "id=\"job-poll\""] {
        assert!(page.body.contains(region), "{region}");
    }

    // The board does not; its regions poll on their own.
    assert!(!dashboard.get("/").await.body.contains("sessionStorage"));
}

/// A job page with all six content cards.
fn to_all_six_cards(dashboard: &Dashboard, id: i64) {
    to_review_turn(dashboard, id);
    dashboard.apply(id, Event::ReviewApproved);
    dashboard
        .store
        .record_mcp_tool_call(id, "context7", "query-docs")
        .unwrap();
    append_a_log_line(dashboard, id);
}

fn section_toggle(section: &str) -> String {
    format!("aria-controls=\"section-{section}\"")
}

#[tokio::test]
async fn the_six_content_cards_carry_a_section_and_a_toggle() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_all_six_cards(&dashboard, id);

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains(&format!("<div class=\"job-main\" data-job=\"{id}\">")));
    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await.body;
    let swapped = ["spec", "plan", "reviews", "mcp-calls", "log"];
    for section in ["request", "spec", "plan", "reviews", "mcp-calls", "log"] {
        let marker = format!("data-section=\"{section}\"");
        assert_eq!(page.matches(&marker).count(), 1, "{section}");
        assert!(page.contains(&section_toggle(section)), "{section}");
        assert!(
            page.contains(&format!("id=\"section-{section}\"")),
            "{section}"
        );
        if swapped.contains(&section) {
            assert_eq!(fragment.matches(&marker).count(), 1, "{section} swapped");
            assert!(fragment.contains(&section_toggle(section)), "{section}");
        }
    }
    // Every card is rendered expanded, and none is hidden.
    assert_eq!(page.matches("class=\"section-toggle\"").count(), 6);
    assert_eq!(page.matches("aria-expanded=\"true\"").count(), 6);
    assert!(!page.contains("aria-expanded=\"false\""));
    assert!(!page.contains("class=\"card-body\" hidden"));
    // The cards are not keyed disclosures for `details-state.js`.
    assert!(!page.contains("data-section=\"spec\" data-key"));

    // The status card and the timeline do not collapse.
    let region = |id: &str, until: &str| {
        let at = page.find(&format!("id=\"{id}\"")).expect(id);
        let start = page[..at].rfind('<').unwrap();
        let end = start + page[start..].find(until).expect(until);
        page[start..end].to_string()
    };
    for (id, slice) in [
        ("job-status", region("job-status", "<aside")),
        ("job-timeline", region("job-timeline", "class=\"job-main\"")),
    ] {
        assert!(slice.len() > 100, "{id} slice is the region");
        assert!(!slice.contains("data-section"), "{id}");
        assert!(!slice.contains("section-toggle"), "{id}");
    }
}

#[tokio::test]
async fn the_edit_link_sits_outside_the_request_toggle() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    let button = page.find("section-toggle").unwrap();
    let close = button + page[button..].find("</button>").unwrap();
    assert!(!page[button..close].contains("/edit"));
    assert!(page.contains(&format!("href=\"/jobs/{id}/edit\"")));
}

#[tokio::test]
async fn only_the_job_page_loads_the_sections_script() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let marker = "factory.job-sections:";
    assert!(
        dashboard
            .get(&format!("/jobs/{id}"))
            .await
            .body
            .contains(marker)
    );
    assert!(!dashboard.get("/").await.body.contains(marker));
}

/// A distinctive line from `local-time.js`.
const LOCAL_TIME_MARKER: &str = "time[datetime][data-format]";

/// Both pages that show times inline `local-time.js`, so a swapped-in fragment
/// is converted to the browser's zone too.
#[tokio::test]
async fn the_board_and_the_job_page_load_the_local_time_script() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    assert!(dashboard.get("/").await.body.contains(LOCAL_TIME_MARKER));
    assert!(
        dashboard
            .get(&format!("/jobs/{id}"))
            .await
            .body
            .contains(LOCAL_TIME_MARKER)
    );
}

/// A poll swaps a region's content in. `local-time.js` converts it on htmx's
/// `afterSwap` and `oobAfterSwap` events, which fire before the browser paints,
/// so no UTC frame shows. Both pages inline the script, so both hook the events.
#[tokio::test]
async fn the_local_time_script_converts_swapped_content_before_paint() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    for body in [
        dashboard.get("/").await.body,
        dashboard.get(&format!("/jobs/{id}")).await.body,
    ] {
        assert!(body.contains("htmx:afterSwap"), "hooks the main swap");
        assert!(body.contains("htmx:oobAfterSwap"), "hooks the oob swap");
    }
}

/// The log line time is wrapped as `time-seconds`, in the full page and in the
/// polled fragment, so a visitor sees it in their own zone either way.
#[tokio::test]
async fn the_log_line_time_is_wrapped_in_a_time_element() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    append_a_log_line(&dashboard, id);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("data-format=\"time-seconds\""));
    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert!(fragment.body.contains("data-format=\"time-seconds\""));
}

/// Timeline step times are wrapped as `time`, in the full page and in the
/// polled fragment.
#[tokio::test]
async fn the_timeline_step_time_is_wrapped_in_a_time_element() {
    let dashboard = dashboard();
    let id = watching_ci(&dashboard).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("data-format=\"time\""));
    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert!(fragment.body.contains("data-format=\"time\""));
}

/// Wrapping a time for display does not change what is stored: a new job's
/// `created_at` is still a plain SQLite UTC timestamp, so the retention query
/// that deletes jobs over 30 days old finds the same jobs as before.
#[tokio::test]
async fn storage_and_retention_stay_in_utc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with(Arc::clone(&store));
    let id = dashboard.new_job().await;

    // The stored value is the plain timestamp, with no display markup leaked in.
    let stored: String = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("SELECT created_at FROM jobs WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(!stored.contains("<time"));
    assert_eq!(stored.len(), "2026-11-20 09:15:00".len());

    // Backdated past the cutoff, the job is due for deletion; recent, it is not.
    set_created_at(&path, id, "2000-01-01 00:00:00");
    assert_eq!(store.ids_older_than(30).unwrap(), vec![id]);
    set_created_at(&path, id, "2999-01-01 00:00:00");
    assert_eq!(store.ids_older_than(30).unwrap(), Vec::<i64>::new());
}

#[tokio::test]
async fn the_job_page_has_no_meta_refresh_in_any_state() {
    let dashboard = dashboard();
    for id in [
        dashboard.new_job().await,
        at_spec_gate(&dashboard).await,
        watching_ci(&dashboard).await,
        merged(&dashboard).await,
    ] {
        let page = dashboard.get(&format!("/jobs/{id}")).await;
        assert!(
            !page.body.contains("http-equiv"),
            "job {id} still has a meta refresh"
        );
    }
}

#[tokio::test]
async fn a_terminal_job_page_renders_no_polling_attribute() {
    let dashboard = dashboard();
    // Cancelled, failed and merged jobs do not change again, so the page does
    // not poll and carries no `every 5s` trigger.
    let cancelled = dashboard.new_job().await;
    dashboard.apply(cancelled, Event::Cancel);
    let failed = failed_without_retry(&dashboard).await;
    let merged = merged(&dashboard).await;

    for id in [cancelled, failed, merged] {
        let body = dashboard.get(&format!("/jobs/{id}")).await.body;
        assert!(body.contains("id=\"job-poll\""), "job {id} lost the slot");
        assert!(!body.contains(&poll_attr(id)), "job {id} still polls");
        assert!(
            !body.contains("hx-trigger=\"every 5s\""),
            "job {id} still polls"
        );
    }
}

#[tokio::test]
async fn the_job_page_polls_at_a_gate_and_while_watching_ci() {
    let dashboard = dashboard();
    // A gate and a CI watch are not agent turns, yet a state change can still
    // land there, so both keep polling.
    for id in [
        at_spec_gate(&dashboard).await,
        watching_ci(&dashboard).await,
    ] {
        let body = dashboard.get(&format!("/jobs/{id}")).await.body;
        assert!(body.contains(&poll_attr(id)), "job {id} does not poll");
    }
}

#[tokio::test]
async fn the_job_fragment_is_a_plain_get_of_only_the_live_regions() {
    let dashboard = dashboard();
    let id = at_spec_gate(&dashboard).await;
    append_a_log_line(&dashboard, id);

    let reply = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert_eq!(reply.status, StatusCode::OK);
    // Only the regions, swapped out of band: no page chrome, no cancel form.
    assert!(!reply.body.contains("<html"));
    assert!(!reply.body.contains("<head"));
    assert!(!reply.body.contains("Cancel job"));
    assert!(reply.body.contains("hx-swap-oob=\"true\""));
    // The gate's spec and approval controls are in the swapped regions.
    assert!(reply.body.contains("id=\"job-status\""));
    assert!(reply.body.contains("Approve spec"));
    assert!(reply.body.contains("Bash: cargo test"));
}

#[tokio::test]
async fn a_fragment_for_a_terminal_job_stops_the_poll() {
    let dashboard = dashboard();
    let id = merged(&dashboard).await;

    // A page opened while the job ran polls on; once the job is terminal the
    // fragment swaps the slot for one with no trigger, which ends the loop.
    let reply = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .body
            .contains("<div id=\"job-poll\" hx-swap-oob=\"true\"></div>")
    );
    assert!(!reply.body.contains(&poll_attr(id)));
}

async fn at_spec_gate(dashboard: &Dashboard) -> i64 {
    let id = dashboard.new_job().await;
    to_spec_approval(dashboard, id);
    id
}

/// A job whose pull request is open and waiting for CI and review, a factory
/// turn rather than an agent one.
async fn watching_ci(dashboard: &Dashboard) -> i64 {
    let id = dashboard.new_job().await;
    to_spec_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::Plan(plan_of_one())));
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 7 });
    id
}

async fn merged(dashboard: &Dashboard) -> i64 {
    let id = watching_ci(dashboard).await;
    dashboard.apply(id, Event::Merged);
    id
}

#[tokio::test]
async fn the_settings_page_shows_and_saves_the_stored_settings() {
    let dashboard = dashboard();
    let page = dashboard.get("/settings/workers").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("name=\"max_workers\""));
    assert!(page.body.contains("value=\"2\""));
    assert!(dashboard.get("/").await.body.contains("href=\"/settings\""));

    let reply = dashboard
        .post(
            "/settings/workers",
            "max_workers=4&worker_image=localhost%2Fw%3A2&worker_memory=6g&worker_cpus=6",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    let reply = dashboard.post("/settings/polling", "poll_seconds=30").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    assert_eq!(
        dashboard.store.settings().unwrap(),
        factory_server::Settings {
            max_workers: 4,
            poll_seconds: 30,
            worker_image: "localhost/w:2".into(),
            worker_memory: "6g".into(),
            worker_cpus: "6".into(),
            gates: Gates::default(),
            ..factory_server::Settings::default()
        }
    );
}

#[tokio::test]
async fn auto_merge_is_a_per_project_setting() {
    let dashboard = dashboard();
    // The per-project form carries the auto-merge toggle, the global form
    // does not.
    let page = dashboard.get("/settings/projects/maestro").await;
    assert!(page.body.contains("name=\"auto_merge\""));
    assert!(
        !dashboard
            .store
            .project_auto_merge("maestro", false)
            .unwrap()
    );

    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&auto_merge=on",
        )
        .await;
    assert!(
        dashboard
            .store
            .project_auto_merge("maestro", false)
            .unwrap()
    );
    assert!(dashboard.settings_pages().await.contains("checked"));

    // Saving again without the checkbox clears it.
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=",
        )
        .await;
    assert!(!dashboard.store.project_auto_merge("maestro", true).unwrap());
}

#[tokio::test]
async fn llm_review_is_a_per_project_setting() {
    let dashboard = dashboard();
    // The per-project form carries the review toggle.
    let page = dashboard.get("/settings/projects/maestro").await;
    assert!(page.body.contains("name=\"llm_review\""));
    assert!(
        !dashboard
            .store
            .project_llm_review("maestro", false)
            .unwrap()
    );

    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&llm_review=on",
        )
        .await;
    assert!(
        dashboard
            .store
            .project_llm_review("maestro", false)
            .unwrap()
    );

    // Saving again without the checkbox clears it.
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=",
        )
        .await;
    assert!(!dashboard.store.project_llm_review("maestro", true).unwrap());
}

#[tokio::test]
async fn the_form_saves_auto_merge_and_review_together_and_leaves_other_projects_alone() {
    let dashboard = two_projects();
    // maestro gets both flags on, tool is left untouched.
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&auto_merge=on&llm_review=on",
        )
        .await;
    assert!(
        dashboard
            .store
            .project_auto_merge("maestro", false)
            .unwrap()
    );
    assert!(
        dashboard
            .store
            .project_llm_review("maestro", false)
            .unwrap()
    );
    // Saving maestro wrote no row for tool: it still reads its defaults.
    assert!(!dashboard.store.project_auto_merge("tool", false).unwrap());
    assert!(!dashboard.store.project_llm_review("tool", false).unwrap());

    // Turning tool's review on leaves maestro's flags as they were.
    dashboard
        .post(
            "/settings/projects/tool",
            "clarify=&spec=&plan=&implement=&review=&llm_review=on",
        )
        .await;
    assert!(dashboard.store.project_llm_review("tool", false).unwrap());
    assert!(!dashboard.store.project_auto_merge("tool", true).unwrap());
    assert!(
        dashboard
            .store
            .project_auto_merge("maestro", false)
            .unwrap()
    );
    assert!(
        dashboard
            .store
            .project_llm_review("maestro", false)
            .unwrap()
    );
}

#[tokio::test]
async fn bad_settings_are_rejected_and_not_saved() {
    let dashboard = dashboard();
    let before = dashboard.store.settings().unwrap();
    for (uri, form) in [
        ("/settings/polling", "poll_seconds=5"),
        (
            "/settings/workers",
            "max_workers=2&worker_image=+&worker_memory=3g&worker_cpus=4",
        ),
        (
            "/settings/workers",
            "max_workers=-1&worker_image=i&worker_memory=3g&worker_cpus=4",
        ),
    ] {
        let reply = dashboard.post(uri, form).await;
        assert!(
            reply.status.is_client_error(),
            "{uri} {form}: {}",
            reply.status
        );
    }
    assert_eq!(dashboard.store.settings().unwrap(), before);
}

fn ready() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.".into(),
    }
}

#[tokio::test]
async fn a_pull_request_in_review_waits_for_the_user() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![PlannedPr {
            title: "maestrod: add a reboot endpoint".into(),
            summary: "POST /reboot".into(),
            number: None,
            description: None,
            notes: None,
        }])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("factory working"));
    assert!(!page.body.contains("in review"));

    dashboard.apply(id, Event::Published { number: 15 });
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("class=\"badge you\">in review"));
    assert!(
        page.body
            .contains("#15 waits for CI and your review on GitHub")
    );
    assert!(!page.body.contains("factory working"));

    let index = dashboard.get("/").await;
    assert!(index.body.contains("class=\"inbox inbox-you\""));

    dashboard
        .store
        .save_project_auto_merge("maestro", true)
        .unwrap();
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("merges when green"));
    assert!(page.body.contains("Auto-merge is on"));
    let index = dashboard.get("/").await;
    assert!(!index.body.contains("class=\"inbox inbox-you\""));
}

/// Drives a job to a green pull request and starts a reviewer run, so the
/// next event is a review verdict. The job is in `ci-and-review` on a review
/// turn.
fn to_review_turn(dashboard: &Dashboard, id: i64) {
    to_spec_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::Plan(plan_of_one())));
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 15 });
    dashboard.apply(id, start_review());
}

#[tokio::test]
async fn the_job_page_shows_an_approved_review_pass() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review_turn(&dashboard, id);
    dashboard.apply(id, Event::ReviewApproved);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("LLM review"));
    assert!(page.body.contains("Pass 1"));
    assert!(page.body.contains("approved"));
    // An approval carries no feedback, so no changes text appears.
    assert!(!page.body.contains("changes requested"));
}

#[tokio::test]
async fn the_job_page_shows_review_changes_feedback_escaped() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review_turn(&dashboard, id);
    dashboard.apply(
        id,
        Event::ReviewChanges {
            feedback: "Guard <main> against a null reboot".into(),
        },
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Pass 1"));
    assert!(page.body.contains("changes requested"));
    // The feedback text is HTML-escaped, so angle brackets cannot inject markup.
    assert!(
        page.body
            .contains("Guard &lt;main&gt; against a null reboot")
    );
    assert!(!page.body.contains("Guard <main> against a null reboot"));
}

#[tokio::test]
async fn the_job_page_lists_every_pass_and_the_cap_reason() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review_turn(&dashboard, id);
    // Ask for changes until the review cap stops the job.
    for n in 1..=MAX_REVIEW_ROUNDS {
        dashboard.apply(
            id,
            Event::ReviewChanges {
                feedback: format!("Round {n} feedback"),
            },
        );
        if n < MAX_REVIEW_ROUNDS {
            dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
            dashboard.apply(id, Event::ChecksPassed);
            dashboard.apply(id, Event::Published { number: 15 });
            dashboard.apply(id, start_review());
        }
    }

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    for n in 1..=MAX_REVIEW_ROUNDS {
        assert!(page.body.contains(&format!("Pass {n}")), "pass {n} missing");
        assert!(
            page.body.contains(&format!("Round {n} feedback")),
            "round {n} feedback missing"
        );
    }
    // At the cap the job goes to you with the reason, and does not merge.
    assert!(page.body.contains("class=\"badge you\">needs you"));
    assert!(page.body.contains("The review stopped before merging"));
    assert!(page.body.contains("5 review rounds on one pull request"));
}

#[tokio::test]
async fn the_job_page_shows_a_failed_review_run_reason() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review_turn(&dashboard, id);
    dashboard.apply(
        id,
        Event::ReviewFailed {
            reason: "the reviewer run timed out".into(),
        },
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("class=\"badge you\">needs you"));
    assert!(page.body.contains("The review stopped before merging"));
    assert!(page.body.contains("the reviewer run timed out"));
    // A failed run records no pass, so the passes card does not render.
    assert!(page.body.contains("<div id=\"job-reviews\"></div>"));
}

#[tokio::test]
async fn a_stopped_review_offers_a_retry_that_moves_the_job_on() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review_turn(&dashboard, id);
    dashboard.apply(
        id,
        Event::ReviewFailed {
            reason: "the reviewer run timed out".into(),
        },
    );

    // The review-stopped card offers a Retry that posts to the retry route.
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The review stopped before merging"));
    assert!(page.body.contains(&format!("action=\"/jobs/{id}/retry\"")));
    assert!(page.body.contains(">Retry<"));
    assert!(matches!(
        dashboard.turn(id),
        Turn::User(Ask::ReviewStopped { .. })
    ));

    // Posting it reruns the reviewer, so the job leaves the review-stopped ask.
    let reply = dashboard.post(&format!("/jobs/{id}/retry"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(!matches!(
        dashboard.turn(id),
        Turn::User(Ask::ReviewStopped { .. })
    ));
    assert_eq!(dashboard.turn(id), Turn::Agent(Task::Review));
}

#[tokio::test]
async fn a_renovate_job_shows_on_the_board_and_its_page_and_retries_from_it() {
    let dashboard = dashboard();
    let url = "https://github.com/Badger-Embedded/maestro/pull/7";
    let id = dashboard
        .store
        .create_renovate_job("maestro", 7, "Update dependency serde", url)
        .unwrap()
        .unwrap();
    dashboard.apply(
        id,
        Event::RenovateObserved {
            head: "abc1234".into(),
            ci: factory_core::RenovateCi::Failed { log: "boom".into() },
            limits: factory_core::RetryLimits { auto: 0, total: 0 },
            auto_merge: false,
            access: Box::default(),
        },
    );
    dashboard.apply(id, Event::Commented);

    let board = dashboard.get("/").await.body;
    assert!(board.contains("Update dependency serde"));
    assert!(board.contains("Renovate"));
    assert!(board.contains(&format!("href=\"{url}\"")));
    assert!(board.contains("title=\"factory\""));

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains("Triage stopped"));
    assert!(page.contains("Reruns on this head commit: 0 of 7"));
    assert!(page.contains(&format!("href=\"{url}\"")));
    assert!(page.contains(&format!("action=\"/jobs/{id}/retry\"")));

    let reply = dashboard.post(&format!("/jobs/{id}/retry"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(matches!(dashboard.turn(id), Turn::RerunCi { .. }));
}

fn stopped_renovate_job(dashboard: &Dashboard) -> i64 {
    let id = dashboard
        .store
        .create_renovate_job(
            "maestro",
            7,
            "Update dependency serde",
            "https://github.com/Badger-Embedded/maestro/pull/7",
        )
        .unwrap()
        .unwrap();
    dashboard.apply(
        id,
        Event::RenovateObserved {
            head: "abc1234".into(),
            ci: factory_core::RenovateCi::Failed { log: "boom".into() },
            limits: factory_core::RetryLimits { auto: 0, total: 0 },
            auto_merge: false,
            access: Box::default(),
        },
    );
    dashboard.apply(id, Event::Commented);
    id
}

#[tokio::test]
async fn the_fix_form_shows_only_on_a_stopped_renovate_job() {
    let dashboard = dashboard();
    let stopped = stopped_renovate_job(&dashboard);
    let page = dashboard.get(&format!("/jobs/{stopped}")).await.body;
    assert!(page.contains(&format!("action=\"/jobs/{stopped}/fix\"")));
    assert!(page.contains("name=\"instructions\""));
    assert!(page.contains("Fix in a separate pull request"));
    assert!(page.contains(&format!("action=\"/jobs/{stopped}/retry\"")));

    let ready = dashboard
        .store
        .create_renovate_job("maestro", 8, "Update tokio", "https://example/pull/8")
        .unwrap()
        .unwrap();
    renovate_to_ready(&dashboard, ready, "h1");
    let page = dashboard.get(&format!("/jobs/{ready}")).await.body;
    assert!(page.contains("Ready to merge"));
    assert!(!page.contains("/fix\""));

    let unsafe_job = dashboard
        .store
        .create_renovate_job("maestro", 9, "Update axum", "https://example/pull/9")
        .unwrap()
        .unwrap();
    observe_renovate(&dashboard, unsafe_job, "h1", passed_with_details());
    dashboard.apply(
        unsafe_job,
        Event::Agent(AgentResult::DiffVerdict(Box::new(
            factory_core::DiffVerdict {
                safe: false,
                reason: "drops a feature".into(),
                summary: "Drops a feature".into(),
                from: None,
                to: None,
            },
        ))),
    );
    dashboard.apply(unsafe_job, Event::Commented);
    let page = dashboard.get(&format!("/jobs/{unsafe_job}")).await.body;
    assert!(page.contains("The update is not safe to merge"));
    assert!(!page.contains("/fix\""));
}

fn first_value(html: &str, selector: &str, attr: &str) -> String {
    scraper::Html::parse_document(html)
        .select(&scraper::Selector::parse(selector).unwrap())
        .next()
        .and_then(|node| node.value().attr(attr))
        .unwrap_or_else(|| panic!("no {selector}"))
        .to_owned()
}

fn timeline_text(html: &str) -> String {
    scraper::Html::parse_document(html)
        .select(&scraper::Selector::parse("div.timeline").unwrap())
        .next()
        .expect("the page has a timeline")
        .text()
        .collect()
}

async fn post_fix(dashboard: &Dashboard, login: &str, id: i64, instructions: &str) -> Reply {
    let page = dashboard.get_as(login, &format!("/jobs/{id}")).await.body;
    let csrf = first_value(&page, ".fix-form input[name=csrf]", "value");
    dashboard
        .post_as(
            login,
            &format!("/jobs/{id}/fix"),
            &format!("instructions={instructions}&csrf={csrf}"),
        )
        .await
}

#[tokio::test]
async fn a_fix_post_without_a_valid_token_creates_nothing() {
    let dashboard = dashboard();
    let id = stopped_renovate_job(&dashboard);
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    let token = first_value(&page, ".fix-form input[name=csrf]", "value");
    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await.body;
    let polled = first_value(&fragment, ".fix-form input[name=csrf]", "value");
    assert!(!token.is_empty() && !polled.is_empty());

    dashboard
        .store
        .add_collaborator("maestro", "bob@example.com", "test")
        .unwrap();
    let other = dashboard
        .get_as("bob@example.com", &format!("/jobs/{id}"))
        .await
        .body;
    let others = first_value(&other, ".fix-form input[name=csrf]", "value");
    for form in [
        "instructions=x".to_owned(),
        "instructions=x&csrf=".to_owned(),
        "instructions=x&csrf=forged".to_owned(),
        format!("instructions=x&csrf={others}"),
    ] {
        let reply = dashboard.post(&format!("/jobs/{id}/fix"), &form).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{form}");
    }
    assert!(dashboard.store.get(id + 1).is_err());

    let accepted = dashboard
        .post(
            &format!("/jobs/{id}/fix"),
            &format!("instructions=x&csrf={polled}"),
        )
        .await;
    assert_eq!(accepted.status, StatusCode::SEE_OTHER);
    assert!(dashboard.store.get(id + 1).is_ok());
}

#[tokio::test]
async fn starting_a_fix_creates_one_linked_job_and_leaves_needs_you() {
    let dashboard = dashboard();
    let id = stopped_renovate_job(&dashboard);

    let stranger = dashboard
        .post_as(STRANGER, &format!("/jobs/{id}/fix"), "instructions=x")
        .await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    assert!(dashboard.store.get(id + 1).is_err());

    let reply = post_fix(&dashboard, USER, id, "update+the+smoke+test").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let fix = dashboard.store.get(id + 1).unwrap();
    assert_eq!(fix.requested_by, USER);
    assert_eq!(*fix.job.stage(), Stage::Requested);
    assert_eq!(fix.job.fix_source(), Some((id, 7)));
    for part in [
        "#7",
        "https://github.com/Badger-Embedded/maestro/pull/7",
        "update the smoke test",
    ] {
        assert!(fix.job.request().contains(part), "{part}");
    }

    let again = dashboard
        .post(&format!("/jobs/{id}/fix"), "instructions=&csrf=stale")
        .await;
    assert!(matches!(
        again.status,
        StatusCode::CONFLICT | StatusCode::FORBIDDEN
    ));
    assert!(dashboard.store.get(id + 2).is_err());

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains(&format!(
        "Waiting for fix job <a href=\"/jobs/{0}\">#{0}</a>",
        fix.id
    )));
    assert!(!page.contains("/retry\""));
    assert!(!page.contains("/fix\" "));
    assert!(timeline_text(&page).contains(&format!("Started fix job #{}", fix.id)));
    let note: String = scraper::Html::parse_document(&page)
        .select(&scraper::Selector::parse("div.timeline ul.fix-notes li").unwrap())
        .next()
        .expect("the timeline has a fix note")
        .text()
        .collect();
    assert!(
        note.starts_with(&format!("Started fix job #{} ", fix.id)),
        "{note}"
    );

    let fix_page = dashboard.get(&format!("/jobs/{}", fix.id)).await.body;
    assert!(fix_page.contains(&format!("<a href=\"/jobs/{id}\">job {id}</a>")));

    let board = dashboard.get("/").await.body;
    assert!(board.contains("Running <span class=\"count\">"));
    let you = board.split("inbox inbox-you").nth(1).unwrap_or_default();
    assert!(
        !you.split("</section>")
            .next()
            .unwrap_or_default()
            .contains("Update dependency serde")
    );
    let notes = notifications_as(&dashboard, USER).await;
    assert!(
        notes["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["job"] != id)
    );
}

#[tokio::test]
async fn a_second_fix_post_is_a_conflict() {
    let dashboard = dashboard();
    let id = stopped_renovate_job(&dashboard);
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    let csrf = first_value(&page, ".fix-form input[name=csrf]", "value");
    let form = format!("instructions=&csrf={csrf}");
    let first = dashboard.post(&format!("/jobs/{id}/fix"), &form).await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    let second = dashboard.post(&format!("/jobs/{id}/fix"), &form).await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert!(dashboard.store.get(id + 2).is_err());
}

#[tokio::test]
async fn a_cancelled_fix_job_brings_back_the_fix_form() {
    let dashboard = dashboard();
    let id = stopped_renovate_job(&dashboard);
    post_fix(&dashboard, USER, id, "").await;
    dashboard.apply(
        id,
        Event::FixEnded {
            job: id + 1,
            outcome: factory_core::FixOutcome::Cancelled,
        },
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains("Triage stopped"));
    assert!(page.contains(&format!("action=\"/jobs/{id}/retry\"")));
    assert!(page.contains(&format!("action=\"/jobs/{id}/fix\"")));
    let line = scraper::Html::parse_document(&page)
        .select(&scraper::Selector::parse("p.status-detail").unwrap())
        .next()
        .unwrap()
        .inner_html();
    assert!(line.contains("<br>"), "{line}");
    assert!(line.contains(&format!("Fix job #{} was cancelled", id + 1)));
    assert!(timeline_text(&page).contains(&format!("Fix job #{} was cancelled", id + 1)));
}

#[tokio::test]
async fn a_failed_rebase_request_shows_under_needs_you() {
    let dashboard = dashboard();
    let id = stopped_renovate_job(&dashboard);
    post_fix(&dashboard, USER, id, "").await;
    dashboard.apply(
        id,
        Event::FixEnded {
            job: id + 1,
            outcome: factory_core::FixOutcome::Merged {
                head: "abc1234".into(),
            },
        },
    );
    dashboard.apply(
        id,
        Event::RebaseFailed {
            reason: "no checkbox".into(),
        },
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains("could not ask Renovate to rebase. Rebase the pull request by hand."));
    assert!(!page.contains("/fix\" "));
    let timeline = timeline_text(&page);
    assert!(timeline.contains("Could not ask Renovate to rebase: no checkbox"));
    assert!(timeline.contains(&format!("Fix job #{} merged", id + 1)));
    let board = dashboard.get("/").await.body;
    assert!(board.contains("Needs you <span class=\"count\">1</span>"));
}

#[tokio::test]
async fn a_fix_link_to_a_reused_job_id_is_plain_text() {
    let dashboard = dashboard();
    let id = stopped_renovate_job(&dashboard);
    post_fix(&dashboard, USER, id, "").await;
    let fix = id + 1;

    dashboard
        .store
        .delete_job(fix, Path::new("/nonexistent"))
        .unwrap();
    let other = dashboard.new_job().await;
    assert_eq!(other, fix);
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(!page.contains(&format!("href=\"/jobs/{fix}\"")));
    assert!(page.contains("a job that is no longer available"));
}

#[tokio::test]
async fn a_fix_job_back_link_to_a_reused_job_id_is_plain_text() {
    let dashboard = dashboard();
    let id = stopped_renovate_job(&dashboard);
    post_fix(&dashboard, USER, id, "").await;
    let fix = id + 1;

    dashboard
        .store
        .delete_job(id, Path::new("/nonexistent"))
        .unwrap();
    let page = dashboard.get(&format!("/jobs/{fix}")).await.body;
    assert!(!page.contains(&format!("href=\"/jobs/{id}\"")));
    assert!(page.contains("a job that is no longer available"));
}

#[tokio::test]
async fn a_merged_renovate_job_page_keeps_the_verdict_and_summary() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_renovate_job("maestro", 7, "Update serde", "https://example/pull/7")
        .unwrap()
        .unwrap();
    dashboard.apply(
        id,
        Event::RenovateObserved {
            head: "abc1234".into(),
            ci: factory_core::RenovateCi::Passed {
                details: Some(Box::new(factory_core::PrDiff {
                    diff: "+serde = 1.0.2".into(),
                    body: "Bumps serde".into(),
                })),
            },
            limits: factory_core::RetryLimits { auto: 3, total: 7 },
            auto_merge: false,
            access: Box::default(),
        },
    );
    dashboard.apply(
        id,
        Event::Agent(AgentResult::DiffVerdict(Box::new(
            factory_core::DiffVerdict {
                safe: true,
                reason: "only a version bump".into(),
                summary: "Bumps serde to 1.0.2".into(),
                from: None,
                to: None,
            },
        ))),
    );
    dashboard.apply(id, Event::Merged);

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains("Diff check: safe"));
    assert!(page.contains("Summary: Bumps serde to 1.0.2"));
}

#[tokio::test]
async fn the_job_page_has_no_review_card_without_passes() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review_turn(&dashboard, id);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(!page.body.contains("LLM review"));
}

#[tokio::test]
async fn the_job_fragment_swaps_the_review_passes() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review_turn(&dashboard, id);
    dashboard.apply(id, Event::ReviewApproved);

    // A poll updates the passes out of band, so a new pass shows without a
    // reload.
    let reply = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("id=\"job-reviews\""));
    assert!(reply.body.contains("Pass 1"));
}

fn planned(title: &str, number: Option<u64>) -> PlannedPr {
    PlannedPr {
        title: title.into(),
        summary: "summary".into(),
        number,
        description: None,
        notes: None,
    }
}

async fn with_plan(dashboard: &Dashboard, plan: Vec<PlannedPr>) -> i64 {
    let id = dashboard.new_job().await;
    to_spec_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::Plan(plan)));
    id
}

#[tokio::test]
async fn the_job_page_links_the_project_to_its_repository() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains(r#"<a href="https://github.com/Badger-Embedded/maestro" target="_blank" rel="noopener noreferrer">maestro</a>"#)
    );
}

#[tokio::test]
async fn the_plan_card_shows_the_notes_for_the_implementer() {
    let dashboard = dashboard();
    let mut noted = planned("add a reboot endpoint", None);
    noted.notes = Some("Change maestrod/src/api.rs.".into());
    let id = with_plan(&dashboard, vec![noted, planned("add a button", None)]).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Notes for the implementer"));
    assert!(page.body.contains("Change maestrod/src/api.rs."));
    assert_eq!(page.body.matches("Notes for the implementer").count(), 1);
}

#[tokio::test]
async fn the_job_page_links_a_numbered_pull_request() {
    let dashboard = dashboard();
    let id = with_plan(&dashboard, vec![planned("add a reboot endpoint", Some(7))]).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains(r#"<a href="https://github.com/Badger-Embedded/maestro/pull/7" target="_blank" rel="noopener noreferrer">#7</a>"#)
    );
    assert_eq!(page.body.matches("target=").count(), 2);
    assert_eq!(page.body.matches("noopener noreferrer").count(), 2);

    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert!(fragment.body.contains(
        r#"<a href="https://github.com/Badger-Embedded/maestro/pull/7" target="_blank" rel="noopener noreferrer">#7</a>"#
    ));
}

#[tokio::test]
async fn external_links_open_in_a_new_tab_and_internal_links_do_not() {
    let dashboard = dashboard();
    let id = with_plan(&dashboard, vec![planned("x", Some(7))]).await;

    support::check_link_targets(&dashboard.get("/").await.body);
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert_eq!(support::check_link_targets(&page.body), 2);
    let fragment = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert!(support::check_link_targets(&fragment.body) >= 1);
    support::check_link_targets(&dashboard.settings_pages().await);
}

#[tokio::test]
async fn only_numbered_pull_requests_in_a_plan_get_links() {
    let dashboard = dashboard();
    let id = with_plan(
        &dashboard,
        vec![planned("first", Some(7)), planned("second", None)],
    )
    .await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert_eq!(page.body.matches("/pull/").count(), 1);
    assert!(page.body.contains("second"));
}

fn without_projects(dashboard: &Dashboard) -> Dashboard {
    let router = app(
        dashboard.store.clone(),
        Arc::new(Projects::default()),
        Arc::new(Notify::new()),
        dashboard.data.path().to_owned(),
        Arc::new(PlanUsageCache::new()),
        Forges::new(Arc::new(GitHub::default()), Arc::new(FakeRepos::fit())),
        Arc::new(WorkerSlots::new()),
        Arc::new(UnavailableSecrets::new(dashboard.store.clone())),
        Arc::new(FakeChecker::default()) as Arc<dyn McpChecker>,
        Arc::new(AgentRegistry::new(
            Arc::new(ClaudeToken::new(
                Arc::new(UnavailableSecrets::new(dashboard.store.clone())),
                None,
            )),
            Arc::new(CodexLogin::new(Arc::new(UnavailableSecrets::new(
                dashboard.store.clone(),
            )))),
            Arc::new(UnavailableSecrets::new(dashboard.store.clone())),
        )),
        Arc::new(DeviceLogin::new(
            Arc::new(UnavailableSecrets::new(dashboard.store.clone())),
            Arc::new(NoLogin),
            std::env::temp_dir(),
        )),
        Arc::new(DeviceLogin::for_spec(
            factory_server::LoginSpec::COPILOT,
            Arc::new(UnavailableSecrets::new(dashboard.store.clone())),
            Arc::new(NoLogin),
            std::env::temp_dir(),
        )),
        vec![USER.to_owned()],
    );
    Dashboard {
        store: dashboard.store.clone(),
        router,
        data: tempfile::tempdir().unwrap(),
        usage: Arc::new(PlanUsageCache::new()),
        slots: Arc::new(WorkerSlots::new()),
        checker: Arc::new(FakeChecker::default()),
    }
}

#[tokio::test]
async fn a_job_whose_project_is_gone_renders_without_links() {
    let dashboard = dashboard();
    let id = with_plan(&dashboard, vec![planned("first", Some(7))]).await;
    let page = without_projects(&dashboard)
        .get(&format!("/jobs/{id}"))
        .await;

    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("maestro"));
    assert!(page.body.contains("#7"));
    assert!(!page.body.contains("github.com"));
}

#[tokio::test]
async fn the_board_links_the_selected_projects_repository() {
    let dashboard = dashboard();
    let page = dashboard.get("/").await;
    let html = scraper::Html::parse_document(&page.body);
    let select = |css: &str| scraper::Selector::parse(css).unwrap();

    let link = html
        .select(&select("div.status-row > a.source-link"))
        .next()
        .expect("the status row holds the source link");
    let attr = |name| link.value().attr(name);
    assert_eq!(
        attr("href"),
        Some("https://github.com/Badger-Embedded/maestro")
    );
    assert_eq!(attr("target"), Some("_blank"));
    assert_eq!(attr("rel"), Some("noopener noreferrer"));
    assert!(
        link.text()
            .collect::<String>()
            .contains("Badger-Embedded/maestro")
    );
    assert!(
        link.select(&select("svg[aria-hidden=true]"))
            .next()
            .is_some()
    );
    assert!(
        html.select(&select("#workers a.source-link, #usage a.source-link"))
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn the_board_has_no_source_link_when_the_project_does_not_resolve() {
    let dashboard = dashboard();
    let page = without_projects(&dashboard).get("/").await;

    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("source-link"));
}

#[tokio::test]
async fn the_polled_fragments_carry_no_source_link() {
    let dashboard = dashboard();
    for path in [
        "/fragments/workers?project=maestro",
        "/fragments/plan-usage?project=maestro",
        "/fragments/board?project=maestro",
    ] {
        assert!(
            !dashboard.get(path).await.body.contains("source-link"),
            "{path}"
        );
    }
}

#[tokio::test]
async fn links_use_the_repository_from_the_project_file() {
    let dashboard = dashboard_for("someone-else/widgets");
    let id = with_plan(&dashboard, vec![planned("first", Some(3))]).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains(r#"href="https://github.com/someone-else/widgets""#)
    );
    assert!(
        page.body
            .contains(r#"href="https://github.com/someone-else/widgets/pull/3""#)
    );
    assert!(!page.body.contains("Badger-Embedded"));
}

#[test]
fn the_forge_migration_moves_every_project_to_github() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                 name TEXT PRIMARY KEY,
                 repo TEXT NOT NULL UNIQUE,
                 base TEXT NOT NULL,
                 checks TEXT,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             INSERT INTO projects (name, repo, base, checks)
                 VALUES ('one', 'o/one', 'main', 'just test'), ('two', 'o/two', 'dev', NULL);",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 2);
    for (_, project) in &projects {
        assert_eq!(project.forge, ProjectForge::Known(ForgeKind::GitHub));
    }
    assert_eq!(projects[0].1.checks.as_deref(), Some("just test"));
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO projects (name, forge, repo, base) VALUES ('three', 'elsewhere', 'o/one', 'main')",
        [],
    )
    .unwrap();
    let duplicate = conn.execute(
        "INSERT INTO projects (name, forge, repo, base) VALUES ('four', 'github', 'o/one', 'main')",
        [],
    );
    assert!(duplicate.is_err());
    drop(conn);
    Store::open(&path).unwrap();
}

#[test]
fn a_repository_is_unique_by_forge_and_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    store
        .create_project("one", &ForgeKind::GitHub, "o/repo", "main", None, None)
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO projects (name, forge, repo, base) VALUES ('other', 'elsewhere', 'o/repo', 'main')",
        [],
    )
    .unwrap();
    let other = store.project("other").unwrap().unwrap();
    assert_eq!(other.forge, ProjectForge::Unknown("elsewhere".to_owned()));
    assert!(
        conn.execute(
            "INSERT INTO projects (name, forge, repo, base) VALUES ('dup', 'elsewhere', 'o/repo', 'main')",
            [],
        )
        .is_err()
    );
}

#[tokio::test]
async fn a_project_on_an_unknown_forge_is_an_error_for_that_project_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO projects (name, forge, repo, base) VALUES ('odd', 'elsewhere', 'o/odd', 'main');
         INSERT INTO project_collaborators (project, login, added_by) VALUES ('odd', 'alice', 'alice');",
    )
    .unwrap();
    let dashboard = dashboard_with(store);

    let board = dashboard.get("/?project=odd").await;
    assert_eq!(board.status, StatusCode::OK);
    assert!(board.body.contains("elsewhere"));
    assert!(!board.body.contains("source-link"));
    let settings = dashboard.get("/settings/projects/odd").await;
    assert_eq!(settings.status, StatusCode::OK);
    assert!(settings.body.contains("does not support"));

    let healthy = dashboard.get("/?project=maestro").await;
    assert_eq!(healthy.status, StatusCode::OK);
    assert!(healthy.body.contains("source-link"));
    assert!(!healthy.body.contains("does not support"));
}

fn two_projects() -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    store
        .create_project(
            "tool",
            &ForgeKind::GitHub,
            "o/tool",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    dashboard_with(store)
}

#[tokio::test]
async fn the_index_defaults_to_the_first_project() {
    let dashboard = two_projects();
    dashboard.new_job().await;
    dashboard.post_job("tool", "Tool change", &[]).await;
    let page = dashboard.get("/").await;
    assert_current_project(&page.body, "maestro", "maestro");
    assert!(page.body.contains("Add a reboot button</a>"));
    assert!(!page.body.contains("Tool change"));
}

#[tokio::test]
async fn the_board_shows_only_the_selected_project() {
    let dashboard = two_projects();
    let maestro = dashboard.new_job().await;
    dashboard.post_job("tool", "Tool change", &[]).await;
    to_spec_approval(&dashboard, maestro);

    let page = dashboard.get("/?project=tool").await;
    assert_eq!(page.status, StatusCode::OK);
    assert_current_project(&page.body, "tool", "tool");
    assert!(page.body.contains("Tool change"));
    assert!(!page.body.contains("Add a reboot button</a>"));
    assert!(!page.body.contains("class=\"inbox inbox-you\""));
    assert!(page.body.contains("New request for tool"));
    assert!(page.body.contains("name=\"project\" value=\"tool\""));

    let page = dashboard.get("/?project=maestro").await;
    assert!(!page.body.contains("Tool change"));
    assert!(page.body.contains("class=\"inbox inbox-you\""));
    assert_eq!(page.body.matches("Add a reboot button</a>").count(), 1);
}

#[tokio::test]
async fn an_unknown_project_in_the_index_is_not_found() {
    let page = dashboard().get("/?project=nope").await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
}

fn project_cookie(reply: &Reply) -> Option<String> {
    reply
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|value| value.starts_with("factory_project="))
        .map(|value| value.split(';').next().unwrap().to_owned())
}

#[tokio::test]
async fn selecting_a_project_sets_the_cookie() {
    let dashboard = two_projects();
    let page = dashboard.get("/?project=tool").await;
    assert_eq!(page.status, StatusCode::OK);
    let cookie = page.header(header::SET_COOKIE).unwrap();
    for part in [
        "factory_project=tool",
        "Path=/",
        "SameSite=Lax",
        "HttpOnly",
        "Max-Age=",
    ] {
        assert!(cookie.contains(part), "{cookie}");
    }
    assert!(!cookie.contains("Secure"));
}

#[tokio::test]
async fn the_cookie_picks_the_project_after_a_visit_elsewhere() {
    let dashboard = two_projects();
    dashboard.new_job().await;
    dashboard.post_job("tool", "Tool change", &[]).await;
    let cookie = project_cookie(&dashboard.get("/?project=tool").await).unwrap();
    let settings = dashboard
        .get_with_cookie(USER, "/settings/workers", &cookie)
        .await;
    assert_eq!(settings.status, StatusCode::OK);
    assert!(project_cookie(&settings).is_none());

    let page = dashboard.get_with_cookie(USER, "/", &cookie).await;
    assert_current_project(&page.body, "tool", "tool");
    assert!(page.body.contains("Tool change"));
    assert!(project_cookie(&page).is_none());
}

#[tokio::test]
async fn an_explicit_project_beats_the_cookie() {
    let dashboard = two_projects();
    let page = dashboard
        .get_with_cookie(USER, "/?project=maestro", "factory_project=tool")
        .await;
    assert_current_project(&page.body, "maestro", "maestro");
    assert_eq!(
        project_cookie(&page).as_deref(),
        Some("factory_project=maestro")
    );
}

#[tokio::test]
async fn a_stale_or_foreign_cookie_falls_back_to_the_first_project() {
    let dashboard = two_projects();
    dashboard
        .store
        .create_project(
            "secret",
            &ForgeKind::GitHub,
            "o/secret",
            "main",
            None,
            Some("bob@example.com"),
        )
        .unwrap();
    for cookie in [
        "factory_project=nope",
        "factory_project=secret",
        "factory_project=%ZZ",
        "factory_project=%FF",
        "other=1; factory_project=",
    ] {
        let page = dashboard.get_with_cookie(USER, "/", cookie).await;
        assert_eq!(page.status, StatusCode::OK, "{cookie}");
        assert_current_project(&page.body, "maestro", "maestro");
        assert!(project_cookie(&page).is_none());
    }
    let page = dashboard
        .get_with_cookie(STRANGER, "/", "factory_project=maestro")
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains("You are not a collaborator on any project.")
    );
}

#[tokio::test]
async fn a_missing_project_sets_no_cookie() {
    let dashboard = two_projects();
    let page = dashboard.get("/?project=nope").await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    assert!(page.header(header::SET_COOKIE).is_none());
    let page = dashboard.get_as(STRANGER, "/?project=maestro").await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    assert!(page.header(header::SET_COOKIE).is_none());
}

#[tokio::test]
async fn the_fragments_set_no_cookie() {
    let dashboard = two_projects();
    for uri in [
        "/fragments/board?project=maestro",
        "/fragments/stats?project=maestro",
        "/fragments/workers?project=maestro",
        "/fragments/plan-usage?project=maestro",
    ] {
        let page = dashboard.get(uri).await;
        assert!(page.header(header::SET_COOKIE).is_none(), "{uri}");
    }
}

#[tokio::test]
async fn an_unusual_project_name_round_trips_through_the_cookie() {
    let name = "my proj; é,x";
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    store
        .create_project(
            name,
            &ForgeKind::GitHub,
            "o/odd",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with(store);
    let uri = "/?project=my%20proj%3B%20%C3%A9%2Cx";
    let cookie = project_cookie(&dashboard.get(uri).await).unwrap();
    let page = dashboard.get_with_cookie(USER, "/", &cookie).await;
    assert_current_project(&page.body, "my%20proj%3B%20%C3%A9%2Cx", "my proj; é,x");
}

const STRANGER: &str = "mallory@example.com";

#[tokio::test]
async fn a_non_collaborators_board_is_the_empty_state() {
    let page = two_projects().get_as(STRANGER, "/").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains("You are not a collaborator on any project.")
    );
    assert!(!page.body.contains("project-picker"));
    assert!(!page.body.contains("maestro"));
    assert!(page.body.contains("href=\"/onboard\""));
}

#[tokio::test]
async fn a_hidden_project_board_and_fragments_are_not_found() {
    let dashboard = two_projects();
    for uri in [
        "/?project=maestro",
        "/fragments/board?project=maestro",
        "/fragments/stats?project=maestro",
        "/fragments/workers?project=maestro",
        "/fragments/picker?project=maestro",
    ] {
        assert_eq!(
            dashboard.get_as(STRANGER, uri).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    let hidden = dashboard.get_as(STRANGER, "/?project=maestro").await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    assert!(hidden.body.contains("no project maestro"));
}

#[tokio::test]
async fn a_non_collaborator_cannot_see_or_act_on_a_job() {
    let dashboard = two_projects();
    let id = dashboard.new_job().await;

    for uri in [
        format!("/jobs/{id}"),
        format!("/jobs/{id}/fragment"),
        format!("/jobs/{id}/edit"),
        format!("/jobs/{id}/images/image-1.png"),
    ] {
        let reply = dashboard.get_as(STRANGER, &uri).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        assert!(reply.body.contains(&format!("no job {id}")), "{uri}");
    }

    for route in ["cancel", "approve", "retry", "dismiss"] {
        let reply = dashboard
            .post_as(STRANGER, &format!("/jobs/{id}/{route}"), "")
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{route}");
    }
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Requested
    );
}

#[tokio::test]
async fn a_non_collaborator_cannot_post_a_job_to_a_project() {
    let dashboard = two_projects();
    let reply = dashboard
        .post_job_as(STRANGER, "maestro", "A change the stranger wants", &[])
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
}

#[tokio::test]
async fn a_hidden_projects_settings_pages_are_not_found() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    for uri in [
        "/settings/projects/maestro",
        "/settings/projects/maestro/mcp/context7",
    ] {
        assert_eq!(
            dashboard.get_as(STRANGER, uri).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    assert_eq!(
        dashboard
            .post_as(STRANGER, "/settings/projects/maestro", "clarify=opus")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dashboard
            .post_as(
                STRANGER,
                "/settings/projects/maestro/mcp/context7/check",
                ""
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(dashboard.checker.calls(), 0);
}

#[tokio::test]
async fn the_settings_sidebar_lists_only_the_logins_own_projects() {
    let dashboard = two_projects();
    dashboard
        .store
        .create_project(
            "stray",
            &ForgeKind::GitHub,
            "o/stray",
            "main",
            None,
            Some(STRANGER),
        )
        .unwrap();
    let body = dashboard.get_as(STRANGER, "/settings/workers").await.body;
    assert!(body.contains("/settings/projects/stray"));
    assert!(!body.contains("/settings/projects/maestro"));
    assert!(!body.contains("/settings/projects/tool"));
}

fn collaborator_logins(dashboard: &Dashboard, project: &str) -> Vec<String> {
    dashboard
        .store
        .collaborators(project)
        .unwrap()
        .into_iter()
        .map(|collaborator| collaborator.login)
        .collect()
}

#[tokio::test]
async fn a_collaborator_adds_a_login_that_then_sees_the_board() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro/collaborators",
            "login=bob@example.com",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Added bob@example.com."));
    assert!(reply.body.contains("bob@example.com"));
    assert!(
        dashboard
            .store
            .is_collaborator("maestro", "bob@example.com")
            .unwrap()
    );

    let board = dashboard.get_as("bob@example.com", "/").await;
    assert_eq!(board.status, StatusCode::OK);
    assert!(board.body.contains("maestro"));
    assert!(
        !board
            .body
            .contains("You are not a collaborator on any project.")
    );
}

#[tokio::test]
async fn a_typed_collaborator_login_is_trimmed() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/projects/maestro/collaborators",
            "login=%20%20bob%40example.com%20%20",
        )
        .await;
    assert!(
        dashboard
            .store
            .is_collaborator("maestro", "bob@example.com")
            .unwrap()
    );
}

#[tokio::test]
async fn an_empty_or_duplicate_collaborator_is_refused() {
    let dashboard = dashboard();
    let before = collaborator_logins(&dashboard, "maestro");

    let empty = dashboard
        .post("/settings/projects/maestro/collaborators", "login=%20%20")
        .await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(
        empty
            .body
            .contains("Type a Tailscale login to add a collaborator.")
    );

    let duplicate = dashboard
        .post("/settings/projects/maestro/collaborators", "login=alice")
        .await;
    assert_eq!(duplicate.status, StatusCode::OK);
    assert!(duplicate.body.contains("alice is already a collaborator."));

    assert_eq!(collaborator_logins(&dashboard, "maestro"), before);
}

#[tokio::test]
async fn a_collaborator_removes_another_while_more_than_one_remains() {
    let dashboard = dashboard();
    assert!(collaborator_logins(&dashboard, "maestro").len() >= 2);

    let reply = dashboard
        .post(
            "/settings/projects/maestro/collaborators/remove",
            "login=alice",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Removed alice."));
    assert!(!dashboard.store.is_collaborator("maestro", "alice").unwrap());

    let hidden = dashboard.get_as("alice", "/?project=maestro").await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn removing_oneself_redirects_to_settings() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro/collaborators/remove",
            &format!("login={USER}"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/settings"));
    assert!(!dashboard.store.is_collaborator("maestro", USER).unwrap());
}

#[tokio::test]
async fn the_last_collaborator_cannot_be_removed() {
    let dashboard = dashboard();
    dashboard
        .store
        .remove_collaborator("maestro", "alice")
        .unwrap();
    assert_eq!(
        collaborator_logins(&dashboard, "maestro"),
        vec![USER.to_owned()]
    );

    let page = dashboard.get("/settings/projects/maestro").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("/collaborators/remove"));

    let reply = dashboard
        .post(
            "/settings/projects/maestro/collaborators/remove",
            &format!("login={USER}"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .body
            .contains("A project needs at least one collaborator. Remove the project instead.")
    );
    assert!(dashboard.store.is_collaborator("maestro", USER).unwrap());
}

#[tokio::test]
async fn a_non_collaborator_cannot_add_or_remove_collaborators() {
    let dashboard = two_projects();
    for route in ["collaborators", "collaborators/remove"] {
        let reply = dashboard
            .post_as(
                STRANGER,
                &format!("/settings/projects/maestro/{route}"),
                "login=bob@example.com",
            )
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{route}");
    }
    assert!(
        !dashboard
            .store
            .is_collaborator("maestro", "bob@example.com")
            .unwrap()
    );
}

const DOM_LIB: &str = r#"
    function parseList(sel) { return sel.split(",").map(function (s) { return s.trim(); }).filter(Boolean); }
    function parseComplex(sel) { return sel.trim().split(" ").filter(Boolean).map(function (c) { return c.split(".").filter(Boolean); }); }
    function matchCompound(node, classes) { for (var i = 0; i < classes.length; i++) { if (node.classes.indexOf(classes[i]) < 0) return false; } return true; }
    function matchComplex(node, compounds) {
      if (!matchCompound(node, compounds[compounds.length - 1])) return false;
      var anc = node.parent, i = compounds.length - 2;
      while (i >= 0) {
        var ok = false;
        while (anc) { if (matchCompound(anc, compounds[i])) { anc = anc.parent; ok = true; break; } anc = anc.parent; }
        if (!ok) return false;
        i--;
      }
      return true;
    }
    function matchesSel(node, sel) { return parseList(sel).some(function (s) { return matchComplex(node, parseComplex(s)); }); }
    function firstDescendant(node, sel) {
      for (var i = 0; i < node.children.length; i++) {
        var c = node.children[i];
        if (matchesSel(c, sel)) return c;
        var d = firstDescendant(c, sel);
        if (d) return d;
      }
      return null;
    }
    var NODES = {};
    RAW.forEach(function (r) { NODES[r.id] = { id: r.id, tag: r.tag, classes: r.classes.slice(), action: r.action, textContent: "", parent: null, children: [] }; });
    RAW.forEach(function (r) { var n = NODES[r.id]; if (r.parent >= 0) { n.parent = NODES[r.parent]; NODES[r.parent].children.push(n); } });
    var ALL = Object.keys(NODES).map(function (k) { return NODES[k]; });
    ALL.forEach(function (node) {
      node.closest = function (sel) { var n = node; while (n) { if (matchesSel(n, sel)) return n; n = n.parent; } return null; };
      node.matches = function (sel) { return matchesSel(node, sel); };
      node.querySelector = function (sel) { return firstDescendant(node, sel); };
      node.classList = { add: function (c) { if (node.classes.indexOf(c) < 0) node.classes.push(c); }, contains: function (c) { return node.classes.indexOf(c) >= 0; } };
    });
    var docListeners = {};
    var winListeners = {};
    var document = {
      addEventListener: function (type, fn) { (docListeners[type] = docListeners[type] || []).push(fn); },
      querySelectorAll: function (sel) { return ALL.filter(function (n) { return matchesSel(n, sel); }); },
    };
    var window = {
      addEventListener: function (type, fn) { (winListeners[type] = winListeners[type] || []).push(fn); },
    };
    function fire(listeners, type, target) {
      var prevented = false;
      var event = { target: target, preventDefault: function () { prevented = true; }, returnValue: null };
      (listeners[type] || []).forEach(function (fn) { fn(event); });
      return prevented;
    }
    var pipelineForm = NODES[pipelineFormId];
    var pipelineInput = NODES[pipelineInputId];
    var collabAddForm = NODES[collabAddFormId];
    var collabRemoveForm = NODES[collabRemoveFormId];
    var maintenanceForm = NODES[maintenanceFormId];
    var maintenanceInput = NODES[maintenanceInputId];
"#;

#[derive(serde::Serialize)]
struct DomNode {
    id: i64,
    parent: i64,
    tag: String,
    classes: Vec<String>,
    action: Option<String>,
}

fn settings_form_setup(body: &str) -> String {
    let doc = scraper::Html::parse_document(body);
    let mut nodes: Vec<DomNode> = Vec::new();
    let mut ids = std::collections::HashMap::new();
    for node in doc.tree.root().descendants() {
        let Some(el) = node.value().as_element() else {
            continue;
        };
        let id = i64::try_from(nodes.len()).unwrap();
        ids.insert(node.id(), id);
        let parent = node
            .parent()
            .and_then(|p| ids.get(&p.id()).copied())
            .unwrap_or(-1);
        let classes = el
            .attr("class")
            .map(|c| c.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        nodes.push(DomNode {
            id,
            parent,
            tag: el.name().to_owned(),
            classes,
            action: el.attr("action").map(str::to_owned),
        });
    }
    let form_posting_to = |action: &str| {
        nodes
            .iter()
            .find(|n| n.tag == "form" && n.action.as_deref() == Some(action))
            .unwrap_or_else(|| panic!("no form posts to {action}"))
            .id
    };
    let pipeline_form_id = nodes
        .iter()
        .find(|n| {
            n.classes.iter().any(|c| c == "settings-form")
                && n.classes.iter().any(|c| c == "pipeline-form")
        })
        .expect("the pipeline settings form")
        .id;
    let descends_from_pipeline = |mut id: i64| {
        while let Ok(index) = usize::try_from(id) {
            if id == pipeline_form_id {
                return true;
            }
            id = nodes[index].parent;
        }
        false
    };
    let maintenance_form_id = form_posting_to("/settings/projects/maestro/maintenance");
    let within = |form: i64, mut id: i64| {
        while let Ok(index) = usize::try_from(id) {
            if id == form {
                return true;
            }
            id = nodes[index].parent;
        }
        false
    };
    let maintenance_input_id = nodes
        .iter()
        .find(|n| n.tag == "input" && within(maintenance_form_id, n.id))
        .expect("an input in the maintenance form")
        .id;
    let pipeline_input_id = nodes
        .iter()
        .find(|n| {
            matches!(n.tag.as_str(), "input" | "select" | "textarea")
                && descends_from_pipeline(n.id)
        })
        .map_or(pipeline_form_id, |n| n.id);
    format!(
        "var RAW = {}; var pipelineFormId = {pipeline_form_id}; var pipelineInputId = {pipeline_input_id}; var collabAddFormId = {}; var collabRemoveFormId = {}; var maintenanceFormId = {maintenance_form_id}; var maintenanceInputId = {maintenance_input_id};\n",
        serde_json::to_string(&nodes).unwrap(),
        form_posting_to("/settings/projects/maestro/collaborators"),
        form_posting_to("/settings/projects/maestro/collaborators/remove"),
    )
}

fn beforeunload_warns_after(setup: &str, actions: &str) -> bool {
    let script = include_str!("../static/settings-form.js");
    let code = format!("{setup}{DOM_LIB}{script}\n{actions}");
    let mut context = boa_engine::Context::default();
    context
        .eval(boa_engine::Source::from_bytes(&code))
        .unwrap()
        .as_boolean()
        .unwrap()
}

#[tokio::test]
async fn a_collaborator_submit_keeps_the_unsaved_pipeline_prompt() {
    let body = dashboard().get("/settings/projects/maestro").await.body;
    let setup = settings_form_setup(&body);
    let dirty_then_submit = |form| {
        format!(
            "fire(docListeners, \"input\", pipelineInput);\
             fire(docListeners, \"submit\", {form});\
             fire(winListeners, \"beforeunload\", null);"
        )
    };
    assert!(beforeunload_warns_after(
        &setup,
        &dirty_then_submit("collabAddForm")
    ));
    assert!(beforeunload_warns_after(
        &setup,
        &dirty_then_submit("collabRemoveForm")
    ));
    assert!(!beforeunload_warns_after(
        &setup,
        &dirty_then_submit("pipelineForm")
    ));
    assert!(!beforeunload_warns_after(
        &setup,
        "fire(winListeners, \"beforeunload\", null);"
    ));
}

#[tokio::test]
async fn maintenance_edits_mark_the_page_unsaved_until_that_form_saves() {
    let body = dashboard().get("/settings/projects/maestro").await.body;
    let setup = settings_form_setup(&body);
    let edit = "fire(docListeners, \"input\", maintenanceInput);";
    let leave = "fire(winListeners, \"beforeunload\", null);";
    let after = |submit: &str| format!("{edit}{submit}{leave}");
    assert!(beforeunload_warns_after(&setup, &after("")));
    assert!(beforeunload_warns_after(
        &setup,
        &after("fire(docListeners, \"submit\", collabAddForm);")
    ));
    assert!(!beforeunload_warns_after(
        &setup,
        &after("fire(docListeners, \"submit\", maintenanceForm);")
    ));
}

#[tokio::test]
async fn saving_one_settings_form_keeps_the_prompt_for_the_other() {
    let body = dashboard().get("/settings/projects/maestro").await.body;
    let setup = settings_form_setup(&body);
    let dirty_then_submit = |input: &str, form: &str| {
        format!(
            "fire(docListeners, \"input\", {input});\
             fire(docListeners, \"submit\", {form});\
             fire(winListeners, \"beforeunload\", null);"
        )
    };
    let both = "fire(docListeners, \"input\", pipelineInput);\
                fire(docListeners, \"input\", maintenanceInput);";
    assert!(beforeunload_warns_after(
        &setup,
        &dirty_then_submit("pipelineInput", "maintenanceForm")
    ));
    assert!(beforeunload_warns_after(
        &setup,
        &dirty_then_submit("maintenanceInput", "pipelineForm")
    ));
    assert!(beforeunload_warns_after(
        &setup,
        &format!(
            "{both}fire(docListeners, \"submit\", pipelineForm);fire(winListeners, \"beforeunload\", null);"
        )
    ));
    assert!(!beforeunload_warns_after(
        &setup,
        &dirty_then_submit("pipelineInput", "pipelineForm")
    ));
}

#[tokio::test]
async fn the_picker_is_in_the_topbar_and_the_form_has_no_project_select() {
    let page = two_projects().get("/?project=tool").await;
    let main = page.body.find("<main>").unwrap();
    let (topbar, rest) = page.body.split_at(main);
    assert!(!page.body.contains("<select"));
    assert!(topbar.contains("<details class=\"project-picker\" id=\"project-picker\">"));
    assert!(
        topbar.contains("<summary role=\"button\" aria-label=\"Project\" aria-expanded=\"false\" aria-keyshortcuts=\"p\">")
    );
    assert!(topbar.contains("<span class=\"picker-current\">tool</span>"));
    assert!(topbar.contains("<a href=\"/?project=maestro\">"));
    assert_current_project(&page.body, "tool", "tool");
    assert!(rest.contains("<input type=\"hidden\" name=\"project\" value=\"tool\">"));
}

#[tokio::test]
async fn the_picker_shows_its_keys_as_kbd_caps() {
    let page = two_projects().get("/?project=tool").await;
    let doc = scraper::Html::parse_document(&page.body);
    let sel = |s: &str| scraper::Selector::parse(s).unwrap();
    let cap = doc
        .select(&sel(
            "#project-picker summary[aria-keyshortcuts=p] > kbd[aria-hidden=true]",
        ))
        .next()
        .unwrap();
    assert_eq!(cap.text().collect::<String>(), "P");
    let footer = doc
        .select(&sel(
            "#project-picker .picker-panel > div.picker-foot > div.picker-keys",
        ))
        .next()
        .unwrap();
    let caps: Vec<String> = footer
        .select(&sel("kbd"))
        .map(|k| k.text().collect::<String>())
        .collect();
    assert_eq!(caps, ["J", "K", "I", "N", "⇧N"]);
    assert!(
        footer.value().attr("hidden").is_some(),
        "hidden until the script runs"
    );
    assert!(cap.value().attr("hidden").is_some());
}

#[tokio::test]
async fn the_picker_has_a_search_box_and_a_hidden_empty_message() {
    let page = two_projects().get("/?project=tool").await;
    let doc = scraper::Html::parse_document(&page.body);
    let one = |css: &str| doc.select(&scraper::Selector::parse(css).unwrap()).next();
    let search = one("#project-picker .picker-panel > input#picker-search").unwrap();
    assert_eq!(search.value().attr("type"), Some("search"));
    assert_eq!(search.value().attr("aria-label"), Some("Find a project"));
    assert!(search.value().attr("role").is_none());
    assert!(one("#project-picker .picker-panel > p.picker-status[role=status]").is_some());
    let empty = one("#project-picker .picker-panel > p.picker-empty").unwrap();
    assert!(empty.value().attr("hidden").is_some());
    assert_eq!(empty.text().collect::<String>(), "No project matches.");
    let rows = doc
        .select(&scraper::Selector::parse("#picker-list li[data-id]").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(row.value().attr("data-id").is_some());
        assert!(row.value().attr("data-name").is_some());
        assert!(row.value().attr("id").unwrap().starts_with("picker-opt-"));
    }
}

#[tokio::test]
async fn the_picker_sits_after_the_brand_and_before_the_nav() {
    let page = two_projects().get("/?project=tool").await;
    let brand = page.body.find("class=\"brand\"").unwrap();
    let picker = page.body.find("<details class=\"project-picker\"").unwrap();
    let topbar_end = page.body.find("class=\"topbar-end\"").unwrap();
    let onboard = page.body.find("href=\"/onboard\"").unwrap();
    assert!(brand < picker, "the picker comes after the brand");
    assert!(picker < topbar_end, "the picker comes before .topbar-end");
    assert!(picker < onboard, "the picker comes before the Onboard link");
}

/// The picker's current row, as its link and the name it shows.
fn current_project(body: &str) -> Option<(String, String)> {
    let doc = scraper::Html::parse_document(body);
    let link = doc
        .select(&scraper::Selector::parse("#picker-list a[aria-current=page]").unwrap())
        .next()?;
    let name = scraper::Selector::parse(".picker-name").unwrap();
    let shown = link.select(&name).next()?.text().collect::<String>();
    Some((link.value().attr("href")?.to_owned(), shown))
}

/// Asserts the picker marks the project with the URL-escaped identifier `id`
/// as current and shows `shown` for it.
fn assert_current_project(body: &str, id: &str, shown: &str) {
    assert_eq!(
        current_project(body),
        Some((format!("/?project={id}"), shown.to_owned()))
    );
}

/// The picker's rows in order: identifier, group, and the text of the badges.
fn picker_rows(body: &str) -> Vec<(String, String, Vec<String>)> {
    let doc = scraper::Html::parse_document(body);
    let selector = |css: &str| scraper::Selector::parse(css).unwrap();
    doc.select(&selector("#picker-list li[data-id]"))
        .map(|row| {
            let badges = row
                .select(&selector(".badge, .picker-running"))
                .map(|badge| badge.text().collect::<String>())
                .collect();
            (
                row.value().attr("data-id").unwrap().to_owned(),
                row.value().attr("data-attention").unwrap().to_owned(),
                badges,
            )
        })
        .collect()
}

/// The jobs in the board's Needs you and Failed sections.
fn board_counts(body: &str) -> (usize, usize) {
    let doc = scraper::Html::parse_document(body);
    let selector = |css: &str| scraper::Selector::parse(css).unwrap();
    let count = |lane: &str| {
        doc.select(&selector(&format!("section.inbox-{lane} h2 .count")))
            .next()
            .map_or(0, |count| count.text().collect::<String>().parse().unwrap())
    };
    (count("you"), count("failed"))
}

fn picker_counts(body: &str, project: &str) -> (usize, usize) {
    let (_, _, badges) = picker_rows(body)
        .into_iter()
        .find(|(id, ..)| id == project)
        .unwrap();
    let number = |suffix: &str| {
        badges
            .iter()
            .find_map(|text| text.strip_suffix(suffix))
            .map_or(0, |count| count.trim().parse().unwrap())
    };
    (number(" need you"), number(" failed"))
}

async fn job_in(dashboard: &Dashboard, project: &str) -> i64 {
    let reply = dashboard.post_job(project, "A change", &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    reply
        .location
        .unwrap()
        .strip_prefix("/jobs/")
        .unwrap()
        .parse()
        .unwrap()
}

fn fail_job(dashboard: &Dashboard, id: i64) {
    dashboard.apply(id, Event::Start);
    dashboard.apply(
        id,
        Event::AgentFailed {
            reason: "boom".into(),
        },
    );
}

#[tokio::test]
async fn picker_counts_equal_the_board_section_counts() {
    let dashboard = dashboard();
    let question = dashboard.new_job().await;
    dashboard.apply(question, Event::Start);
    dashboard.apply(
        question,
        Event::Agent(AgentResult::Questions(vec![Question {
            text: "Which?".into(),
        }])),
    );
    let spec = dashboard.new_job().await;
    to_spec_approval(&dashboard, spec);
    let plan = dashboard.new_job().await;
    to_spec_approval(&dashboard, plan);
    dashboard.apply(plan, Event::Approve);
    dashboard.apply(plan, Event::Agent(AgentResult::Plan(plan_of_one())));
    let review = dashboard.new_job().await;
    to_spec_approval(&dashboard, review);
    dashboard.apply(review, Event::Approve);
    dashboard.apply(review, Event::Agent(AgentResult::Plan(plan_of_one())));
    dashboard.apply(review, Event::Approve);
    dashboard.apply(review, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(review, Event::ChecksPassed);
    dashboard.apply(review, Event::Published { number: 3 });
    let failed = dashboard.new_job().await;
    fail_job(&dashboard, failed);
    let publish = at_publish_gate(&dashboard).await;
    dashboard.apply(publish, Event::Summarized(branch_summary()));
    let dismissed = dashboard.new_job().await;
    fail_job(&dashboard, dismissed);
    dashboard.apply(dismissed, Event::Dismiss);

    let body = dashboard.get("/").await.body;
    assert_eq!(picker_counts(&body, "maestro"), (5, 1));
    assert_eq!(board_counts(&body), (5, 1));

    dashboard
        .store
        .save_project_auto_merge("maestro", true)
        .unwrap();
    let body = dashboard.get("/").await.body;
    assert_eq!(picker_counts(&body, "maestro"), (4, 1));
    assert_eq!(board_counts(&body), (4, 1));
    assert!(body.contains(
        "<span class=\"badge you\">4<span class=\"visually-hidden\"> need you</span></span>"
    ));
    assert!(body.contains(
        "<span class=\"badge bad\">1<span class=\"visually-hidden\"> failed</span></span>"
    ));
}

fn four_projects() -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    for name in ["a-idle", "b-failed", "c-you", "d-running"] {
        store
            .create_project(
                name,
                &ForgeKind::GitHub,
                &format!("o/{name}"),
                "main",
                None,
                Some("alice"),
            )
            .unwrap();
    }
    dashboard_with(store)
}

#[tokio::test]
async fn the_picker_orders_needs_you_then_failed_then_running_then_the_rest() {
    let dashboard = four_projects();
    let failed = job_in(&dashboard, "b-failed").await;
    fail_job(&dashboard, failed);
    let you = job_in(&dashboard, "c-you").await;
    to_spec_approval(&dashboard, you);
    job_in(&dashboard, "d-running").await;

    let body = dashboard.get("/?project=a-idle").await.body;
    let rows = picker_rows(&body);
    let order: Vec<_> = rows.iter().map(|(id, ..)| id.as_str()).collect();
    assert_eq!(order, ["c-you", "b-failed", "d-running", "a-idle"]);
    let groups: Vec<_> = rows.iter().map(|(_, group, _)| group.as_str()).collect();
    assert_eq!(groups, ["you", "failed", "running", "idle"]);
    assert_eq!(rows[3].2, Vec::<String>::new());
    assert_eq!(rows[2].2, ["running"]);
}

/// The picker's sections in order: the heading, or `None` for a section
/// without one, and the identifiers of its rows.
fn picker_sections(body: &str) -> Vec<(Option<String>, Vec<String>)> {
    let doc = scraper::Html::parse_document(body);
    let selector = |css: &str| scraper::Selector::parse(css).unwrap();
    doc.select(&selector("#picker-list > li.picker-section"))
        .map(|section| {
            let heading = section
                .select(&selector(".picker-heading"))
                .next()
                .map(|heading| heading.text().collect::<String>());
            let rows = section
                .select(&selector("li[data-id]"))
                .map(|row| row.value().attr("data-id").unwrap().to_owned())
                .collect();
            (heading, rows)
        })
        .collect()
}

#[tokio::test]
async fn the_picker_labels_its_attention_sections_and_leaves_the_rest_unlabelled() {
    let dashboard = four_projects();
    let failed = job_in(&dashboard, "b-failed").await;
    fail_job(&dashboard, failed);
    let you = job_in(&dashboard, "c-you").await;
    to_spec_approval(&dashboard, you);
    job_in(&dashboard, "d-running").await;

    let body = dashboard.get("/?project=a-idle").await.body;
    assert_eq!(
        picker_sections(&body),
        [
            (Some("Needs you".to_owned()), vec!["c-you".to_owned()]),
            (Some("Failed".to_owned()), vec!["b-failed".to_owned()]),
            (Some("Running".to_owned()), vec!["d-running".to_owned()]),
            (None, vec!["a-idle".to_owned()]),
        ]
    );
    assert!(body.contains("<ul aria-labelledby=\"picker-sec-0\">"));
}

#[tokio::test]
async fn the_picker_lists_a_logins_groups_in_name_order_then_other() {
    let dashboard = four_projects();
    let you = job_in(&dashboard, "c-you").await;
    to_spec_approval(&dashboard, you);
    let store = &dashboard.store;
    store.save_project_groups(USER, &[], Some("zeta")).unwrap();
    store.save_project_groups(USER, &[], Some("Alpha")).unwrap();
    let groups = store.project_groups(USER).unwrap();
    let names: Vec<_> = groups.iter().map(|group| group.name.as_str()).collect();
    assert_eq!(names, ["Alpha", "zeta"]);
    let (alpha, zeta) = (groups[0].id, groups[1].id);
    store
        .set_project_groups(
            USER,
            &[
                ("a-idle".into(), Some(zeta)),
                ("c-you".into(), Some(zeta)),
                ("b-failed".into(), Some(alpha)),
            ],
        )
        .unwrap();

    let body = dashboard.get("/?project=a-idle").await.body;
    assert_eq!(
        picker_sections(&body),
        [
            (Some("Alpha".to_owned()), vec!["b-failed".to_owned()]),
            (
                Some("zeta".to_owned()),
                vec!["c-you".to_owned(), "a-idle".to_owned()]
            ),
            (Some("Other".to_owned()), vec!["d-running".to_owned()]),
        ]
    );
    store
        .add_collaborator("a-idle", "bob@example.com", "alice")
        .unwrap();
    let other = dashboard
        .get_as("bob@example.com", "/?project=a-idle")
        .await
        .body;
    assert_eq!(picker_sections(&other), [(None, vec!["a-idle".to_owned()])]);
}

/// The form token on a page rendered for `login`.
fn form_token(body: &str) -> String {
    body.split("name=\"csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn the_project_groups_page_adds_renames_removes_and_assigns_groups() {
    let dashboard = four_projects();
    let page = dashboard.get("/settings/groups").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains("href=\"/settings/groups\" aria-current=\"page\"")
    );
    assert!(
        !page.body.contains("id=\"group-members\""),
        "no projects form before a group exists"
    );
    let csrf = form_token(&page.body);

    let forbidden = dashboard.post("/settings/groups", "new=Home").await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    assert_eq!(dashboard.store.project_groups(USER).unwrap(), []);

    let saved = dashboard
        .post("/settings/groups", &format!("csrf={csrf}&new=+Home+"))
        .await;
    assert!(saved.body.contains("Saved."));
    assert!(saved.body.contains("id=\"group-members\""));
    let home = dashboard.store.project_groups(USER).unwrap()[0].id;

    let repeat = dashboard
        .post(
            "/settings/groups",
            &format!("csrf={csrf}&group-{home}=Home&new=home"),
        )
        .await;
    assert!(
        repeat
            .body
            .contains("Two groups are named &quot;home&quot;, so nothing was saved.")
    );
    let long = "x".repeat(41);
    let too_long = dashboard
        .post("/settings/groups", &format!("csrf={csrf}&new={long}"))
        .await;
    assert!(too_long.body.contains("is longer than 40 characters"));
    assert_eq!(dashboard.store.project_groups(USER).unwrap().len(), 1);

    let assigned = dashboard
        .post(
            "/settings/groups/projects",
            &format!("csrf={csrf}&project%3Aa-idle={home}&project%3Ab-failed=999&project%3Anot-mine={home}"),
        )
        .await;
    assert!(assigned.body.contains("Saved."));
    assert_eq!(
        dashboard.store.project_groups(USER).unwrap()[0].projects,
        ["a-idle"]
    );

    dashboard
        .post(
            "/settings/groups",
            &format!("csrf={csrf}&group-{home}=Hobby"),
        )
        .await;
    let groups = dashboard.store.project_groups(USER).unwrap();
    assert_eq!(
        (groups[0].name.as_str(), groups[0].projects.len()),
        ("Hobby", 1)
    );

    dashboard
        .post("/settings/groups", &format!("csrf={csrf}&group-{home}="))
        .await;
    assert_eq!(dashboard.store.project_groups(USER).unwrap(), []);
    let body = dashboard.get("/?project=a-idle").await.body;
    assert_eq!(
        picker_sections(&body).len(),
        1,
        "no group left, so the list is flat"
    );
}

#[tokio::test]
async fn removing_a_project_drops_it_from_every_group() {
    let dashboard = four_projects();
    let store = &dashboard.store;
    store.save_project_groups(USER, &[], Some("Home")).unwrap();
    let home = store.project_groups(USER).unwrap()[0].id;
    store
        .set_project_groups(
            USER,
            &[("a-idle".into(), Some(home)), ("c-you".into(), Some(home))],
        )
        .unwrap();
    store.remove_project("a-idle").unwrap();
    assert_eq!(store.project_groups(USER).unwrap()[0].projects, ["c-you"]);
}

#[tokio::test]
async fn a_group_id_that_is_not_the_logins_leaves_a_project_where_it_is() {
    let dashboard = four_projects();
    let store = &dashboard.store;
    store.save_project_groups(USER, &[], Some("Home")).unwrap();
    store
        .save_project_groups("someone@else", &[], Some("Theirs"))
        .unwrap();
    let home = store.project_groups(USER).unwrap()[0].id;
    let theirs = store.project_groups("someone@else").unwrap()[0].id;
    store
        .set_project_groups(USER, &[("a-idle".into(), Some(home))])
        .unwrap();
    let csrf = form_token(&dashboard.get("/settings/groups").await.body);

    for foreign in [999, theirs] {
        dashboard
            .post(
                "/settings/groups/projects",
                &format!("csrf={csrf}&project%3Aa-idle={foreign}"),
            )
            .await;
        assert_eq!(store.project_groups(USER).unwrap()[0].projects, ["a-idle"]);
        assert_eq!(
            store.project_groups("someone@else").unwrap()[0]
                .projects
                .len(),
            0
        );
    }
    dashboard
        .post(
            "/settings/groups/projects",
            &format!("csrf={csrf}&project%3Aa-idle="),
        )
        .await;
    assert_eq!(store.project_groups(USER).unwrap()[0].projects.len(), 0);
}

#[tokio::test]
async fn the_group_routes_reject_a_missing_token_and_ignore_another_logins_group() {
    let dashboard = four_projects();
    let store = &dashboard.store;
    store.save_project_groups(USER, &[], Some("Home")).unwrap();
    store
        .save_project_groups("someone@else", &[], Some("Theirs"))
        .unwrap();
    let home = store.project_groups(USER).unwrap()[0].id;
    let theirs = store.project_groups("someone@else").unwrap()[0].id;
    let csrf = form_token(&dashboard.get("/settings/groups").await.body);

    let forbidden = dashboard
        .post(
            "/settings/groups/projects",
            &format!("project%3Aa-idle={home}"),
        )
        .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    assert_eq!(store.project_groups(USER).unwrap()[0].projects.len(), 0);

    dashboard
        .post(
            "/settings/groups",
            &format!("csrf={csrf}&group-{theirs}=Mine"),
        )
        .await;
    assert_eq!(
        store.project_groups("someone@else").unwrap()[0].name,
        "Theirs"
    );
}

#[tokio::test]
async fn a_repeated_group_field_is_checked_as_it_is_saved() {
    let dashboard = four_projects();
    let store = &dashboard.store;
    store.save_project_groups(USER, &[], Some("Home")).unwrap();
    let home = store.project_groups(USER).unwrap()[0].id;
    let csrf = form_token(&dashboard.get("/settings/groups").await.body);
    let long = "x".repeat(41);
    dashboard
        .post(
            "/settings/groups",
            &format!("csrf={csrf}&group-{home}=Hobby&group-{home}={long}"),
        )
        .await;
    assert_eq!(store.project_groups(USER).unwrap()[0].name, "Hobby");
}

#[tokio::test]
async fn the_picker_escapes_group_names_and_its_fragment_lists_the_sections() {
    let dashboard = four_projects();
    let store = &dashboard.store;
    let before = dashboard.get("/fragments/picker?project=a-idle").await.body;
    store
        .save_project_groups(USER, &[], Some("<b>Home</b>"))
        .unwrap();
    let home = store.project_groups(USER).unwrap()[0].id;
    store
        .set_project_groups(USER, &[("a-idle".into(), Some(home))])
        .unwrap();

    let page = dashboard.get("/?project=a-idle").await.body;
    assert!(page.contains("&lt;b&gt;Home&lt;/b&gt;"));
    assert!(!page.contains("<b>Home</b>"));
    let fragment = dashboard.get("/fragments/picker?project=a-idle").await;
    assert_eq!(fragment.status, StatusCode::OK);
    assert!(fragment.body.contains("picker-section"));
    assert!(fragment.body.contains("&lt;b&gt;Home&lt;/b&gt;"));
    assert_ne!(picker_poll_url(&before), picker_poll_url(&fragment.body));
}

fn picker_poll_url(body: &str) -> Option<String> {
    let doc = scraper::Html::parse_fragment(body);
    let list = doc
        .select(&scraper::Selector::parse("#picker-list").unwrap())
        .next()?;
    let url = list.value().attr("hx-get")?;
    Some(url.to_owned())
}

#[tokio::test]
async fn renaming_a_populated_group_invalidates_a_grouped_poll() {
    let dashboard = four_projects();
    let store = &dashboard.store;
    store.save_project_groups(USER, &[], Some("Home")).unwrap();
    let home = store.project_groups(USER).unwrap()[0].id;
    store
        .set_project_groups(USER, &[("a-idle".into(), Some(home))])
        .unwrap();
    let url =
        picker_poll_url(&dashboard.get("/fragments/picker?project=a-idle").await.body).unwrap();
    assert_eq!(dashboard.get(&url).await.status, StatusCode::NO_CONTENT);

    let csrf = form_token(&dashboard.get("/settings/groups").await.body);
    dashboard
        .post(
            "/settings/groups",
            &format!("csrf={csrf}&group-{home}=Hobby"),
        )
        .await;
    let stale = dashboard.get(&url).await;
    assert_eq!(stale.status, StatusCode::OK);
    assert!(stale.body.contains(">Hobby<"));
    assert!(stale.body.contains("hx-swap-oob=\"true\""));
}

#[tokio::test]
async fn a_login_that_is_not_an_admin_reaches_the_project_groups_page() {
    let dashboard = four_projects();
    dashboard
        .store
        .add_collaborator("a-idle", "bob@example.com", "alice")
        .unwrap();
    let page = dashboard
        .get_as("bob@example.com", "/settings/groups")
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("href=\"/settings/groups\""));
}

#[tokio::test]
async fn the_picker_lists_and_counts_only_the_projects_the_login_collaborates_on() {
    let dashboard = two_projects();
    dashboard
        .store
        .add_collaborator("tool", MAESTRO_ONLY, USER)
        .unwrap();
    let id = job_in(&dashboard, "tool").await;
    fail_job(&dashboard, id);
    let hidden = job_in(&dashboard, "maestro").await;
    fail_job(&dashboard, hidden);

    let body = dashboard.get_as(MAESTRO_ONLY, "/").await.body;
    let rows = picker_rows(&body);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "tool");
    assert_eq!(picker_counts(&body, "tool"), (0, 1));
    assert!(!body.contains("data-id=\"maestro\""));

    let fragment = dashboard
        .get_as(MAESTRO_ONLY, "/fragments/picker?project=tool")
        .await;
    assert_eq!(fragment.status, StatusCode::OK);
    assert!(!fragment.body.contains("maestro"));
    let hidden = dashboard
        .get_as(MAESTRO_ONLY, "/fragments/picker?project=maestro")
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
}

fn badge_label(body: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(body);
    let badge = doc
        .select(&scraper::Selector::parse("#picker-badge").unwrap())
        .next()
        .unwrap();
    badge.value().attr("aria-label").map(str::to_string)
}

#[tokio::test]
async fn the_badge_totals_the_other_projects_attention() {
    let dashboard = four_projects();
    let failed = job_in(&dashboard, "b-failed").await;
    fail_job(&dashboard, failed);
    for name in ["c-you", "c-you", "d-running"] {
        let id = job_in(&dashboard, name).await;
        to_spec_approval(&dashboard, id);
    }
    let own = job_in(&dashboard, "a-idle").await;
    to_spec_approval(&dashboard, own);

    let body = dashboard.get("/?project=a-idle").await.body;
    assert_eq!(
        badge_label(&body).as_deref(),
        Some("3 need you in 2 other projects, 1 failed in 1 other project")
    );
    assert!(body.contains("<span class=\"badge you\">3</span><span class=\"badge bad\">1</span>"));
    let fragment = dashboard.get("/fragments/picker?project=a-idle").await.body;
    assert!(fragment.contains("hx-swap-oob=\"true\""));
    assert_eq!(badge_label(&fragment), badge_label(&body));
}

#[tokio::test]
async fn the_badge_is_hidden_without_attention_elsewhere() {
    let dashboard = four_projects();
    let own = job_in(&dashboard, "c-you").await;
    to_spec_approval(&dashboard, own);
    let body = dashboard.get("/?project=c-you").await.body;
    assert_eq!(badge_label(&body), None);
    assert!(body.contains("<span id=\"picker-badge\" hidden></span>"));
    let one = job_in(&dashboard, "b-failed").await;
    fail_job(&dashboard, one);
    let body = dashboard.get("/?project=c-you").await.body;
    assert_eq!(
        badge_label(&body).as_deref(),
        Some("1 failed in 1 other project")
    );
}

#[tokio::test]
async fn the_badge_ignores_projects_the_login_does_not_collaborate_on() {
    let dashboard = two_projects();
    dashboard
        .store
        .add_collaborator("tool", MAESTRO_ONLY, USER)
        .unwrap();
    let hidden = job_in(&dashboard, "maestro").await;
    fail_job(&dashboard, hidden);
    let body = dashboard.get_as(MAESTRO_ONLY, "/").await.body;
    assert_eq!(badge_label(&body), None);
    let fragment = dashboard
        .get_as(MAESTRO_ONLY, "/fragments/picker?project=tool")
        .await;
    assert_eq!(badge_label(&fragment.body), None);
}

#[tokio::test]
async fn a_recovery_poll_without_a_project_counts_every_project_and_polls_unselected() {
    let dashboard = four_projects();
    let id = job_in(&dashboard, "a-idle").await;
    to_spec_approval(&dashboard, id);
    let other = job_in(&dashboard, "c-you").await;
    to_spec_approval(&dashboard, other);

    let fragment = dashboard.get("/fragments/picker").await;
    assert_eq!(fragment.status, StatusCode::OK);
    assert_eq!(
        badge_label(&fragment.body).as_deref(),
        Some("2 need you in 2 other projects")
    );
    assert!(fragment.body.contains("hx-get=\"/fragments/picker?v="));
    assert!(!fragment.body.contains("aria-current"));
}

#[tokio::test]
async fn the_picker_fragment_answers_204_for_a_matching_version() {
    let dashboard = two_projects();
    let body = dashboard.get("/?project=tool").await.body;
    let doc = scraper::Html::parse_document(&body);
    let list = doc
        .select(&scraper::Selector::parse("#picker-list").unwrap())
        .next()
        .unwrap();
    let url = list.value().attr("hx-get").unwrap().replace("&amp;", "&");
    assert!(url.starts_with("/fragments/picker?project=tool&v="));
    assert_eq!(list.value().attr("hx-trigger"), Some("every 5s"));
    let reply = dashboard.get(&url).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let stale = dashboard.get("/fragments/picker?project=tool&v=old").await;
    assert_eq!(stale.status, StatusCode::OK);
    assert!(stale.body.contains("data-id=\"maestro\""));
}

#[tokio::test]
async fn the_login_pill_carries_the_full_login_in_title() {
    let page = two_projects().get("/?project=tool").await;
    let pill = page.body.find("<span class=\"user\"").unwrap();
    let tag = &page.body[pill..pill + page.body[pill..].find('>').unwrap()];
    assert!(tag.contains("title=\""), "the pill has a title: {tag}");
}

fn saved_images(dashboard: &Dashboard, id: i64) -> Vec<String> {
    let dir = dashboard
        .data
        .path()
        .join("jobs")
        .join(id.to_string())
        .join("images");
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn job_id(reply: &Reply) -> i64 {
    reply
        .location
        .as_deref()
        .unwrap()
        .strip_prefix("/jobs/")
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn a_request_with_images_saves_them_under_the_job() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job(
            "maestro",
            "Match this mockup",
            &[
                ("a.png", PNG),
                ("b.jpg", JPEG),
                ("c.gif", GIF),
                ("d.webp", WEBP),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let id = job_id(&reply);

    let names = ["image-1.png", "image-2.jpg", "image-3.gif", "image-4.webp"];
    assert_eq!(dashboard.store.images(id).unwrap(), names);
    assert_eq!(saved_images(&dashboard, id), names);
    let saved = dashboard
        .data
        .path()
        .join("jobs")
        .join(id.to_string())
        .join("images");
    assert_eq!(fs::read(saved.join("image-1.png")).unwrap(), PNG);
}

#[tokio::test]
async fn a_text_only_request_saves_no_images() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    assert_eq!(dashboard.store.images(id).unwrap().len(), 0);
    assert_eq!(saved_images(&dashboard, id).len(), 0);
}

#[tokio::test]
async fn an_empty_file_part_with_no_name_is_ignored() {
    let dashboard = dashboard();
    let reply = dashboard.post_job("maestro", "x", &[("", b"")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(dashboard.store.images(job_id(&reply)).unwrap().len(), 0);
}

#[tokio::test]
async fn upload_file_names_never_reach_the_file_system() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job(
            "maestro",
            "x",
            &[("../../../escape.png", PNG), ("..\\..\\evil.png", PNG)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let id = job_id(&reply);
    assert_eq!(
        dashboard.store.images(id).unwrap(),
        ["image-1.png", "image-2.png"]
    );
    assert_eq!(saved_images(&dashboard, id), ["image-1.png", "image-2.png"]);
    let outside: Vec<_> = fs::read_dir(dashboard.data.path()).unwrap().collect();
    assert_eq!(outside.len(), 1, "only jobs/ exists in the data directory");
}

#[tokio::test]
async fn an_attachment_is_classified_by_content_not_name_or_type() {
    let dashboard = dashboard();
    // A PNG named as text is stored as an image. A text file named as a PNG,
    // and a ZIP file named as text, are stored by their bytes.
    let reply = dashboard
        .post_job(
            "maestro",
            "x",
            &[
                ("photo.txt", PNG),
                ("notes.png", b"a log line"),
                ("data.txt", ZIP),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let id = job_id(&reply);
    assert_eq!(dashboard.store.images(id).unwrap(), ["image-1.png"]);
    let files = dashboard.store.files(id).unwrap();
    assert_eq!(files[0].name, "file-1.txt");
    assert_eq!(files[0].original_name, "notes.png");
    assert_eq!(files[1].name, "file-2.zip");
    assert_eq!(files[1].original_name, "data.txt");
}

#[tokio::test]
async fn a_rejected_type_names_the_accepted_types_and_creates_no_job() {
    let dashboard = dashboard();
    // A PDF, a file with a NUL byte, bytes that are neither UTF-8 nor an image
    // or ZIP, and a named zero-byte upload are each rejected.
    for bytes in [
        // A PDF carries a high-byte comment after its header, so it is not
        // valid UTF-8.
        b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".as_slice(),
        b"text\0with nul",
        &[0xFF, 0xFE, 0x41, 0x00],
        b"",
    ] {
        let reply = dashboard
            .post_job("maestro", "x", &[("f.bin", bytes)])
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(reply.body.contains("ZIP archive"), "{:?}", reply.body);
    }
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
    assert!(!dashboard.data.path().join("jobs").exists());
}

#[tokio::test]
async fn an_oversized_image_is_rejected_with_the_limit() {
    let dashboard = dashboard();
    let mut big = PNG.to_vec();
    big.resize(factory_server::MAX_ATTACHMENT_BYTES + 1, 0);
    let reply = dashboard
        .post_job("maestro", "x", &[("big.png", &big)])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply
            .body
            .contains(&factory_server::MAX_ATTACHMENT_BYTES.to_string())
    );
    assert_eq!(dashboard.store.list().unwrap().len(), 0);

    let mut exact = PNG.to_vec();
    exact.resize(factory_server::MAX_ATTACHMENT_BYTES, 0);
    let reply = dashboard
        .post_job("maestro", "x", &[("ok.png", &exact)])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn too_many_attachments_are_rejected_with_the_limit() {
    let dashboard = dashboard();
    // A mix of images and files counts against one limit.
    let mut attachments: Vec<(&str, &[u8])> = vec![("a.png", PNG), ("b.txt", TEXT), ("c.zip", ZIP)];
    while attachments.len() < factory_server::MAX_ATTACHMENTS + 1 {
        attachments.push(("a.png", PNG));
    }
    let reply = dashboard.post_job("maestro", "x", &attachments).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains(&format!(
        "at most {} attachments",
        factory_server::MAX_ATTACHMENTS
    )));
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
    assert!(!dashboard.data.path().join("jobs").exists());

    let attachments = &attachments[..factory_server::MAX_ATTACHMENTS];
    let reply = dashboard.post_job("maestro", "x", attachments).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn images_without_text_are_rejected() {
    let dashboard = dashboard();
    let reply = dashboard.post_job("maestro", "  ", &[("a.png", PNG)]).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("the request is empty"));
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
    assert!(!dashboard.data.path().join("jobs").exists());
}

#[tokio::test]
async fn the_form_posts_multipart_with_an_image_input() {
    let page = dashboard().get("/").await;
    assert!(page.body.contains("multipart/form-data"));
    assert!(page.body.contains(r#"name="images""#));
}

/// The on-disk file names under `<job>/<sub>`, sorted. An absent directory
/// reads as empty.
fn saved_names(dashboard: &Dashboard, id: i64, sub: &str) -> Vec<String> {
    let dir = dashboard
        .data
        .path()
        .join("jobs")
        .join(id.to_string())
        .join(sub);
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn job_dir_file(dashboard: &Dashboard, id: i64, sub: &str, name: &str) -> Vec<u8> {
    fs::read(
        dashboard
            .data
            .path()
            .join("jobs")
            .join(id.to_string())
            .join(sub)
            .join(name),
    )
    .unwrap()
}

#[tokio::test]
async fn the_edit_form_shows_the_text_and_a_remove_box_per_attachment() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job(
            "maestro",
            "Add a reboot button",
            &[("a.png", PNG), ("b.txt", TEXT)],
        )
        .await;
    let id = job_id(&reply);

    let page = dashboard.get(&format!("/jobs/{id}/edit")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Add a reboot button"));
    assert!(page.body.contains(r#"name="remove" value="image-1.png""#));
    assert!(page.body.contains(r#"name="remove" value="file-1.txt""#));
    assert!(page.body.contains(r#"name="images""#));
}

#[tokio::test]
async fn the_edit_form_redirects_once_the_job_has_started() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);

    let page = dashboard.get(&format!("/jobs/{id}/edit")).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(
        page.location.as_deref(),
        Some(format!("/jobs/{id}").as_str())
    );
}

#[tokio::test]
async fn an_edit_with_no_attachment_change_replaces_only_the_text() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let reply = dashboard
        .post_edit(id, "Add a shutdown button", &[], &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some(format!("/jobs/{id}").as_str())
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Add a shutdown button"));
    assert!(!page.body.contains("Add a reboot button"));

    let events = dashboard.store.events(id).unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.event, "edit");
    assert_eq!(last.stage, "requested");
}

#[tokio::test]
async fn an_edit_removes_an_image_and_renumbers_the_rest() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job(
            "maestro",
            "Match this",
            &[("a.png", PNG), ("b.png", PNG_B), ("c.txt", TEXT)],
        )
        .await;
    let id = job_id(&reply);

    let reply = dashboard
        .post_edit(id, "Match this", &["image-1.png"], &[("new.png", PNG_C)])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    // The kept image-2 becomes image-1, the new upload becomes image-2.
    assert_eq!(
        dashboard.store.images(id).unwrap(),
        ["image-1.png", "image-2.png"]
    );
    assert_eq!(
        saved_names(&dashboard, id, "images"),
        ["image-1.png", "image-2.png"]
    );
    assert_eq!(job_dir_file(&dashboard, id, "images", "image-1.png"), PNG_B);
    assert_eq!(job_dir_file(&dashboard, id, "images", "image-2.png"), PNG_C);

    // The kept file is untouched and keeps its original name.
    let files = dashboard.store.files(id).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "file-1.txt");
    assert_eq!(files[0].original_name, "c.txt");
    assert_eq!(saved_names(&dashboard, id, "files"), ["file-1.txt"]);
}

#[tokio::test]
async fn an_edit_removes_a_file_and_adds_a_zip() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job("maestro", "x", &[("old.txt", TEXT)])
        .await;
    let id = job_id(&reply);

    let reply = dashboard
        .post_edit(id, "x", &["file-1.txt"], &[("bundle.zip", ZIP)])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    let files = dashboard.store.files(id).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "file-1.zip");
    assert_eq!(files[0].original_name, "bundle.zip");
    assert_eq!(saved_names(&dashboard, id, "files"), ["file-1.zip"]);
    assert_eq!(job_dir_file(&dashboard, id, "files", "file-1.zip"), ZIP);
}

#[tokio::test]
async fn an_edit_that_breaks_a_rule_changes_nothing() {
    // The text, the removes and the new uploads of one rejected edit.
    type Case<'a> = (&'a str, Vec<&'a str>, Vec<(&'a str, &'a [u8])>);

    let big_text = "x".repeat(256 * 1024 + 1);
    let mut big_image = PNG.to_vec();
    big_image.resize(factory_server::MAX_ATTACHMENT_BYTES + 1, 0);
    let pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".as_slice();

    let cases: Vec<Case> = vec![
        ("  ", vec![], vec![]),
        (big_text.as_str(), vec![], vec![]),
        ("x", vec![], vec![("d.png", PNG), ("e.png", PNG_B)]),
        ("x", vec![], vec![("big.png", big_image.as_slice())]),
        ("x", vec![], vec![("doc.pdf", pdf)]),
        ("x", vec!["image-9.png"], vec![]),
    ];

    for (text, removes, images) in cases {
        let dashboard = dashboard();
        let reply = dashboard
            .post_job(
                "maestro",
                "Keep me",
                &[
                    ("a.png", PNG),
                    ("b.png", PNG_B),
                    ("c.txt", TEXT),
                    ("d.txt", TEXT),
                ],
            )
            .await;
        let id = job_id(&reply);
        let before_images = dashboard.store.images(id).unwrap();
        let before_files = dashboard.store.files(id).unwrap();

        let reply = dashboard.post_edit(id, text, &removes, &images).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "text {text:?}");

        assert_eq!(dashboard.store.get(id).unwrap().job.request(), "Keep me");
        assert_eq!(dashboard.store.images(id).unwrap(), before_images);
        assert_eq!(dashboard.store.files(id).unwrap(), before_files);
        assert_eq!(
            saved_names(&dashboard, id, "images"),
            ["image-1.png", "image-2.png"]
        );
        assert_eq!(
            saved_names(&dashboard, id, "files"),
            ["file-1.txt", "file-2.txt"]
        );
    }
}

#[tokio::test]
async fn an_edit_after_the_job_starts_is_a_conflict() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job("maestro", "Add a reboot button", &[("a.png", PNG)])
        .await;
    let id = job_id(&reply);
    dashboard.apply(id, Event::Start);

    let reply = dashboard
        .post_edit(
            id,
            "Add a shutdown button",
            &["image-1.png"],
            &[("b.png", PNG_B)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);

    assert_eq!(
        dashboard.store.get(id).unwrap().job.request(),
        "Add a reboot button"
    );
    assert_eq!(dashboard.store.images(id).unwrap(), ["image-1.png"]);
    assert_eq!(saved_names(&dashboard, id, "images"), ["image-1.png"]);
    assert_eq!(job_dir_file(&dashboard, id, "images", "image-1.png"), PNG);
}

#[tokio::test]
async fn only_a_queued_job_offers_an_edit_link() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains(&format!("/jobs/{id}/edit")));

    dashboard.apply(id, Event::Start);
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(!page.body.contains(&format!("/jobs/{id}/edit")));
}

#[tokio::test]
async fn the_timeline_notes_an_edit() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard
        .post_edit(id, "Add a shutdown button", &[], &[])
        .await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("You edited the request"));
}

/// The ids job `id` waits for, in the order it was told, deleted ones included.
fn waits_for(dashboard: &Dashboard, id: i64) -> Vec<i64> {
    dashboard
        .store
        .prerequisites(id)
        .unwrap()
        .iter()
        .map(|prerequisite| prerequisite.id)
        .collect()
}

/// The `job_prerequisites` row ids of job `id`, which the edit form posts as `keep`.
fn rows_of(dashboard: &Dashboard, id: i64) -> Vec<String> {
    dashboard
        .store
        .prerequisites(id)
        .unwrap()
        .iter()
        .map(|prerequisite| prerequisite.row.to_string())
        .collect()
}

#[tokio::test]
async fn the_edit_form_ticks_the_current_prerequisites_and_offers_the_rest() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    let done = merged(&dashboard).await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Waits", &[&first.to_string()])
            .await,
    );
    let dependent = job_id_of(
        &dashboard
            .post_job_after("maestro", "Waits for the waiting job", &[&id.to_string()])
            .await,
    );

    let page = dashboard.get(&format!("/jobs/{id}/edit")).await.body;
    assert!(page.contains("<legend>Start after</legend>"), "{page}");
    let row = &rows_of(&dashboard, id)[0];
    assert!(
        page.contains(&format!(
            "<input type=\"checkbox\" name=\"keep\" value=\"{row}\" checked>"
        )),
        "{page}"
    );
    let offered = |job: i64| page.contains(&format!("name=\"after\" value=\"{job}\""));
    assert!(offered(second), "{page}");
    assert!(!offered(first), "{page}");
    for hidden in [id, dependent, done] {
        assert!(!offered(hidden), "job {hidden}: {page}");
    }
}

#[tokio::test]
async fn a_current_prerequisite_that_is_no_longer_eligible_stays_listed_with_its_state() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Waits", &[&first.to_string()])
            .await,
    );
    dashboard.apply(first, Event::Cancel);

    let page = dashboard.get(&format!("/jobs/{id}/edit")).await.body;
    let row = &rows_of(&dashboard, id)[0];
    assert!(
        page.contains(&format!(
            "name=\"keep\" value=\"{row}\" checked><span class=\"id\">#{first}</span> Add a reboot button (cancelled)"
        )),
        "{page}"
    );
}

#[tokio::test]
async fn clearing_the_prerequisites_lets_the_job_start() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Waits", &[&first.to_string()])
            .await,
    );
    assert!(dashboard.store.start(id).unwrap().is_none());

    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[], &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(waits_for(&dashboard, id), Vec::<i64>::new());
    assert!(dashboard.store.start(id).unwrap().is_some());
}

#[tokio::test]
async fn an_edit_can_switch_the_job_to_wait_for_another() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Waits", &[&first.to_string()])
            .await,
    );

    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[&second.to_string()], &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(waits_for(&dashboard, id), [second]);
    assert!(dashboard.store.start(id).unwrap().is_none());
}

#[tokio::test]
async fn an_edit_naming_an_invalid_prerequisite_changes_nothing() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let a = dashboard.new_job().await;
    let b = job_id_of(
        &dashboard
            .post_job_after("maestro", "Waits", &[&a.to_string()])
            .await,
    );
    let other = dashboard.store.create("elsewhere", "bob", "other").unwrap();
    let renovate = dashboard
        .store
        .create_renovate_job("maestro", 7, "Update serde", "https://example/pull/7")
        .unwrap()
        .unwrap();
    let proposed = dashboard
        .store
        .create_gap_job("maestro", renovate, 7, "Add a flag")
        .unwrap();

    let own = b.to_string();
    let other = other.to_string();
    let unknown = "9999".to_owned();
    let renovate = renovate.to_string();
    let proposed = proposed.to_string();
    let first = first.to_string();
    let c = job_id_of(&dashboard.post_job_after("maestro", "Last", &[&own]).await);
    for closing in [&own, &c.to_string()] {
        let cycle = dashboard
            .post_edit_waiting(a, "Add a reboot button", &[], &[], &[closing], &[])
            .await;
        assert_eq!(cycle.status, StatusCode::BAD_REQUEST, "{closing}");
    }
    for bad in [&own, &other, &unknown, &renovate, &proposed, "x"] {
        let reply = dashboard
            .post_edit_waiting(b, "Changed", &[], &[], &[&first, bad], &[])
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let unknown_reply = dashboard
        .post_edit_waiting(b, "Changed", &[], &[], &[&unknown], &[])
        .await;
    let other_reply = dashboard
        .post_edit_waiting(b, "Changed", &[], &[], &[&other], &[])
        .await;
    let message = |reply: &Reply, id: &str| {
        let (_, rest) = reply.body.split_once("<p>").unwrap();
        rest.split_once("</p>").unwrap().0.replace(id, "N")
    };
    assert_eq!(
        message(&unknown_reply, &unknown),
        message(&other_reply, &other)
    );

    assert_eq!(waits_for(&dashboard, b), [a]);
    assert_eq!(waits_for(&dashboard, a), Vec::<i64>::new());
    assert_eq!(dashboard.store.get(b).unwrap().job.request(), "Waits");
}

#[tokio::test]
async fn an_edit_after_the_job_starts_leaves_the_prerequisites_alone() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    let done = merged(&dashboard).await;
    let id = job_id_of(
        &dashboard
            .post_job_after("maestro", "Waits", &[&done.to_string()])
            .await,
    );
    dashboard.apply(id, Event::Start);

    let reply = dashboard
        .post_edit_waiting(
            id,
            "Waits",
            &[],
            &[],
            &[&first.to_string(), &second.to_string()],
            &[],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(waits_for(&dashboard, id), [done]);
}

/// Job `id` waiting for a newest job that is then deleted, and a new job that
/// reuses the deleted job's number. SQLite reuses only the highest id.
async fn waiting_for_a_deleted_job(dashboard: &Dashboard) -> (i64, i64, i64) {
    let id = dashboard.new_job().await;
    let first = dashboard.new_job().await;
    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[&first.to_string()], &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    dashboard
        .store
        .delete_job(first, Path::new("/nonexistent"))
        .unwrap();
    let reused = dashboard.new_job().await;
    assert_eq!(reused, first);
    (id, first, reused)
}

#[tokio::test]
async fn an_edit_keeps_or_drops_a_deleted_prerequisite() {
    let dashboard = dashboard();
    let (id, first, reused) = waiting_for_a_deleted_job(&dashboard).await;

    let page = dashboard.get(&format!("/jobs/{id}/edit")).await.body;
    let rows = rows_of(&dashboard, id);
    assert!(
        page.contains(&format!(
            "name=\"keep\" value=\"{}\" checked><span class=\"id\">#{first}</span> Add a reboot button (deleted before it merged)",
            rows[0]
        )),
        "{page}"
    );
    assert!(page.contains(&format!("name=\"after\" value=\"{reused}\">")));

    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[], &[&rows[0]])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(waits_for(&dashboard, id), [first]);
    assert!(dashboard.store.start(id).unwrap().is_none());

    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[], &["9999"])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(rows_of(&dashboard, id), rows);

    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[], &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(waits_for(&dashboard, id), Vec::<i64>::new());
    assert!(dashboard.store.start(id).unwrap().is_some());
}

#[tokio::test]
async fn a_stale_edit_form_does_not_swap_a_deleted_prerequisite_for_a_new_job() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let first = dashboard.new_job().await;
    dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[&first.to_string()], &[])
        .await;
    let form = dashboard.get(&format!("/jobs/{id}/edit")).await.body;
    let rows = rows_of(&dashboard, id);
    assert!(form.contains(&format!("name=\"keep\" value=\"{}\" checked", rows[0])));

    dashboard
        .store
        .delete_job(first, Path::new("/nonexistent"))
        .unwrap();
    let reused = dashboard.new_job().await;
    assert_eq!(reused, first);

    // The original form posts the row it showed, with the picker untouched.
    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[], &[&rows[0]])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(rows_of(&dashboard, id), rows);
    let kept = dashboard.store.prerequisites(id).unwrap();
    assert!(kept[0].job.is_none());
    dashboard.apply(reused, Event::Cancel);
    assert!(dashboard.store.start(id).unwrap().is_none());
}

#[tokio::test]
async fn deleted_prerequisites_with_the_same_number_are_kept_or_dropped_apart() {
    let dashboard = dashboard();
    let (id, first, reused) = waiting_for_a_deleted_job(&dashboard).await;
    dashboard
        .post_edit_waiting(
            id,
            "Waits",
            &[],
            &[],
            &[&reused.to_string()],
            &[&rows_of(&dashboard, id)[0]],
        )
        .await;
    dashboard
        .store
        .delete_job(reused, Path::new("/nonexistent"))
        .unwrap();
    let rows = rows_of(&dashboard, id);
    assert_eq!(rows.len(), 2);
    assert_eq!(waits_for(&dashboard, id), [first, first]);

    let page = dashboard.get(&format!("/jobs/{id}/edit")).await.body;
    for row in &rows {
        assert!(
            page.contains(&format!("name=\"keep\" value=\"{row}\" checked")),
            "{page}"
        );
    }

    let reply = dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[], &[&rows[1]])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(rows_of(&dashboard, id), [rows[1].clone()]);
}

#[tokio::test]
async fn a_prerequisite_deleted_after_it_merged_is_listed_as_merged() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let done = merged(&dashboard).await;
    dashboard
        .post_edit_waiting(id, "Waits", &[], &[], &[&done.to_string()], &[])
        .await;
    dashboard
        .store
        .delete_job(done, Path::new("/nonexistent"))
        .unwrap();

    let page = dashboard.get(&format!("/jobs/{id}/edit")).await.body;
    let row = &rows_of(&dashboard, id)[0];
    let label = page
        .split("name=\"keep\"")
        .find(|part| part.starts_with(&format!(" value=\"{row}\"")))
        .unwrap();
    let label = label.split("</label>").next().unwrap();
    assert!(label.contains("(merged)"), "{label}");
    assert!(!label.contains("deleted before it merged"), "{label}");
}

#[tokio::test]
async fn the_timeline_notes_a_change_to_the_prerequisites() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let id = dashboard.new_job().await;
    dashboard
        .post_edit_waiting(
            id,
            "Add a reboot button",
            &[],
            &[],
            &[&first.to_string()],
            &[],
        )
        .await;
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(
        page.contains(&format!("You changed the jobs this waits for: #{first}")),
        "{page}"
    );

    dashboard
        .post_edit_waiting(id, "Add a reboot button", &[], &[], &[], &[])
        .await;
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(
        page.contains("You changed the jobs this waits for: none"),
        "{page}"
    );
}

fn gate_form(gates: &str) -> String {
    gates.trim_start_matches('&').to_owned()
}

#[tokio::test]
async fn the_settings_page_has_a_checkbox_per_gate_with_spec_and_plan_on() {
    let dashboard = dashboard();
    let body = dashboard.settings_pages().await;
    assert!(body.contains("Default gates"));
    for name in ["gate_spec", "gate_plan", "gate_publish"] {
        assert!(body.contains(&format!("name=\"{name}\"")), "{name}");
    }
    assert!(body.contains(r#"name="gate_spec" value="on" checked"#));
    assert!(body.contains(r#"name="gate_plan" value="on" checked"#));
    assert!(!body.contains(r#"name="gate_publish" value="on" checked"#));
    assert!(!body.contains("no effect yet"));
}

#[tokio::test]
async fn the_gate_checkboxes_save_and_an_absent_box_means_off() {
    let dashboard = dashboard();
    let all = gate_form("&gate_spec=on&gate_plan=on&gate_publish=on");
    assert_eq!(
        dashboard.post("/settings/gates", &all).await.status,
        StatusCode::OK
    );
    assert_eq!(
        dashboard.store.settings().unwrap().gates,
        Gates {
            spec: true,
            plan: true,
            publish: true
        }
    );

    dashboard
        .post("/settings/gates", &gate_form("&gate_plan=on"))
        .await;
    assert_eq!(
        dashboard.store.settings().unwrap().gates,
        Gates {
            spec: false,
            plan: true,
            publish: false
        }
    );

    dashboard.post("/settings/gates", &gate_form("")).await;
    let gates = dashboard.store.settings().unwrap().gates;
    assert_eq!(
        gates,
        Gates {
            spec: false,
            plan: false,
            publish: false
        }
    );
    let body = dashboard.settings_pages().await;
    let defaults = factory_form(&body);
    assert!(!defaults.contains(r#"name="gate_spec" value="on" checked"#));
    assert!(!defaults.contains(r#"name="gate_plan" value="on" checked"#));
}

/// The default gates form.
fn factory_form(body: &str) -> &str {
    let start = body.find(r#"action="/settings/gates""#).unwrap();
    let end = start + body[start..].find("</form>").unwrap();
    &body[start..end]
}

/// The form on the settings page that posts to `project`'s settings.
fn project_form<'a>(body: &'a str, project: &str) -> &'a str {
    let start = body
        .find(&format!(r#"action="/settings/projects/{project}?stage="#))
        .unwrap();
    let end = start + body[start..].find("</form>").unwrap();
    &body[start..end]
}

fn no_gates() -> Gates {
    Gates {
        spec: false,
        plan: false,
        publish: false,
    }
}

#[tokio::test]
async fn the_settings_page_calls_the_factory_gates_defaults() {
    let body = dashboard().get("/settings/gates").await.body;
    assert!(body.contains("<h1>Default gates</h1>"));
    assert!(body.contains("A project you onboard starts with these."));
}

#[tokio::test]
async fn each_project_form_shows_its_own_gates() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_gates(
            "maestro",
            Gates {
                spec: false,
                plan: true,
                publish: true,
            },
        )
        .unwrap();
    let body = dashboard.settings_pages().await;
    let form = project_form(&body, "maestro");
    assert!(form.contains(r#"<input type="hidden" name="gates" value="on">"#));
    assert!(form.contains(r#"name="gate_spec" value="on" data-gate="spec""#));
    assert!(form.contains(r#"name="gate_plan" value="on" checked"#));
    assert!(form.contains(r#"name="gate_publish" value="on" checked"#));
}

#[tokio::test]
async fn the_project_form_saves_its_gates_and_an_absent_box_means_off() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&gates=on&gate_publish=on",
        )
        .await;
    assert_eq!(
        dashboard.store.project_gates("maestro").unwrap(),
        Gates {
            spec: false,
            plan: false,
            publish: true,
        }
    );
    // The factory defaults are untouched.
    assert_eq!(dashboard.store.settings().unwrap().gates, Gates::default());
}

#[tokio::test]
async fn a_project_form_without_the_gates_field_keeps_the_gates() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=",
        )
        .await;
    assert_eq!(
        dashboard.store.project_gates("maestro").unwrap(),
        Gates::default()
    );
}

#[test]
fn onboarding_copies_the_default_gates_and_later_defaults_leave_the_project_alone() {
    let store = Store::open_in_memory().unwrap();
    let defaults = Gates {
        spec: true,
        plan: false,
        publish: true,
    };
    store
        .save_settings(&factory_server::Settings {
            gates: defaults,
            ..factory_server::Settings::default()
        })
        .unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    assert_eq!(store.project_gates("maestro").unwrap(), defaults);

    store
        .save_settings(&factory_server::Settings {
            gates: no_gates(),
            ..factory_server::Settings::default()
        })
        .unwrap();
    assert_eq!(store.project_gates("maestro").unwrap(), defaults);
    let id = store.create("maestro", "alice", "a request").unwrap();
    assert_eq!(store.get(id).unwrap().job.gates(), defaults);
}

#[test]
fn the_gates_migration_copies_the_global_gates_into_each_project_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    for (name, repo) in [("maestro", "o/maestro"), ("factory", "o/factory")] {
        store
            .create_project(name, &ForgeKind::GitHub, repo, "main", None, Some("alice"))
            .unwrap();
    }
    // A database from before per-project gates has projects but no gate rows.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM project_gates", [])
        .unwrap();
    let global = Gates {
        spec: false,
        plan: true,
        publish: true,
    };
    store
        .save_settings(&factory_server::Settings {
            gates: global,
            ..factory_server::Settings::default()
        })
        .unwrap();

    store.migrate_project_gates().unwrap();
    assert_eq!(store.project_gates("maestro").unwrap(), global);
    assert_eq!(store.project_gates("factory").unwrap(), global);

    // A later run keeps a project's own gates.
    store.save_project_gates("maestro", no_gates()).unwrap();
    store
        .save_settings(&factory_server::Settings::default())
        .unwrap();
    store.migrate_project_gates().unwrap();
    assert_eq!(store.project_gates("maestro").unwrap(), no_gates());
    assert_eq!(store.project_gates("factory").unwrap(), global);
}

#[test]
fn onboarding_makes_the_onboarding_login_the_only_collaborator() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("factory.db")).unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();

    let collaborators = store.collaborators("maestro").unwrap();
    assert_eq!(collaborators.len(), 1);
    assert_eq!(collaborators[0].login, "alice");
    assert_eq!(collaborators[0].added_by, "alice");
    assert!(store.is_collaborator("maestro", "alice").unwrap());
    assert!(!store.is_collaborator("maestro", "bob").unwrap());
    assert_eq!(store.projects_for("alice").unwrap(), vec!["maestro"]);
    assert_eq!(store.projects_for("bob").unwrap().len(), 0);
}

#[test]
fn adding_and_removing_collaborators_tracks_who_added_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("factory.db")).unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();

    store.add_collaborator("maestro", "bob", "alice").unwrap();
    store.add_collaborator("maestro", "bob", "carol").unwrap();
    let collaborators = store.collaborators("maestro").unwrap();
    let logins: Vec<_> = collaborators.iter().map(|c| c.login.as_str()).collect();
    assert_eq!(logins, ["alice", "bob"]);
    assert_eq!(collaborators[1].added_by, "alice");
    assert_eq!(store.projects_for("bob").unwrap(), vec!["maestro"]);

    store.remove_collaborator("maestro", "alice").unwrap();
    assert!(!store.is_collaborator("maestro", "alice").unwrap());
    assert_eq!(store.projects_for("alice").unwrap().len(), 0);

    let err = store.remove_collaborator("maestro", "bob").unwrap_err();
    assert!(matches!(err, StoreError::LastCollaborator));
    assert_eq!(
        err.to_string(),
        "A project needs at least one collaborator. Remove the project instead."
    );
    assert!(store.is_collaborator("maestro", "bob").unwrap());

    store.remove_collaborator("maestro", "nobody").unwrap();
    assert_eq!(store.collaborators("maestro").unwrap().len(), 1);
}

#[test]
fn the_collaborator_migration_seeds_each_project_from_its_requesters_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    for (name, repo) in [
        ("maestro", "o/maestro"),
        ("factory", "o/factory"),
        ("empty", "o/empty"),
    ] {
        store
            .create_project(
                name,
                &ForgeKind::GitHub,
                repo,
                "main",
                None,
                Some("onboarder"),
            )
            .unwrap();
    }
    store.create("maestro", "alice", "one request").unwrap();
    store.create("maestro", "bob", "another request").unwrap();
    store.create("maestro", "alice", "a third request").unwrap();
    store.create("factory", "carol", "a request").unwrap();

    // A database from before collaborators has projects and jobs but no
    // collaborator rows.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM project_collaborators", [])
        .unwrap();

    store.migrate_project_collaborators().unwrap();

    let logins = |project: &str| {
        let mut logins: Vec<_> = store
            .collaborators(project)
            .unwrap()
            .into_iter()
            .map(|c| c.login)
            .collect();
        logins.sort();
        logins
    };
    assert_eq!(logins("maestro"), ["alice", "bob"]);
    assert_eq!(logins("factory"), ["carol"]);
    assert_eq!(store.collaborators("empty").unwrap().len(), 0);
    assert_eq!(
        store.collaborators("maestro").unwrap()[0].added_by,
        "migration"
    );

    store.remove_collaborator("maestro", "bob").unwrap();
    store.add_collaborator("maestro", "dave", "alice").unwrap();
    store.migrate_project_collaborators().unwrap();
    assert_eq!(logins("maestro"), ["alice", "dave"]);
    assert_eq!(store.collaborators("empty").unwrap().len(), 0);
}

#[tokio::test]
async fn the_job_page_shows_the_gates_the_job_started_with() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let page = dashboard.get(&format!("/jobs/{first}")).await;
    assert!(page.body.contains("Approval gates: spec, plan"));

    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&gates=on&gate_publish=on",
        )
        .await;
    let second = dashboard.new_job().await;
    let page = dashboard.get(&format!("/jobs/{second}")).await;
    assert!(page.body.contains("Approval gates: publish"));
    let page = dashboard.get(&format!("/jobs/{first}")).await;
    assert!(page.body.contains("Approval gates: spec, plan"));

    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&gates=on",
        )
        .await;
    let third = dashboard.new_job().await;
    let page = dashboard.get(&format!("/jobs/{third}")).await;
    assert!(page.body.contains("Approval gates: none"));
}

#[test]
fn a_job_keeps_the_gates_it_was_created_with() {
    let store = Store::open_in_memory().unwrap();
    let before = store.create("maestro", "alice", "before").unwrap();
    let changed = Gates {
        spec: false,
        plan: true,
        publish: true,
    };
    store
        .save_settings(&factory_server::Settings {
            gates: changed,
            ..factory_server::Settings::default()
        })
        .unwrap();
    let after = store.create("maestro", "alice", "after").unwrap();
    assert_eq!(store.get(before).unwrap().job.gates(), Gates::default());
    assert_eq!(store.get(after).unwrap().job.gates(), changed);
}

#[test]
fn a_settings_record_without_gates_loads_the_default_gates() {
    let stored = r#"{"max_workers":4,"worker_image":"i","worker_memory":"3g","worker_cpus":"4","poll_seconds":60,"auto_merge":true}"#;
    let settings: factory_server::Settings = serde_json::from_str(stored).unwrap();
    assert_eq!(settings.max_workers, 4);
    assert_eq!(settings.gates, Gates::default());
}

#[tokio::test]
async fn the_form_has_the_paste_script_and_a_thumbnail_list() {
    let page = dashboard().get("/").await;
    assert!(
        page.body
            .contains(r#"<ul class="pasted-images" id="pasted-images">"#)
    );
    assert!(page.body.contains(r#"getElementById("request")"#));
    assert!(page.body.contains("paste"));
    assert!(page.body.contains("Remove"));
    assert!(
        page.body
            .contains(r#"<textarea id="request" name="request" required"#)
    );
    assert!(
        page.body
            .contains(r#"<input id="images" type="file" name="images" multiple"#)
    );
    assert!(
        page.body
            .contains(r#"<label class="file-button" for="images">Attach files</label>"#)
    );
    assert!(page.body.contains(r#"class="visually-hidden">"#));
    // The accept attribute is gone, since text files have no fixed extensions.
    assert!(!page.body.contains("accept="));
    assert!(page.body.contains("text files or ZIP archives"));
}

#[tokio::test]
async fn a_press_in_the_composer_attachment_row_keeps_the_form_open() {
    let dashboard = dashboard();
    let page = dashboard.get("/").await;
    assert!(
        page.body
            .contains(r#"<div class="composer-more" tabindex="-1">"#)
    );

    let response = dashboard
        .router
        .clone()
        .oneshot(Request::get("/style.css").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let css = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(css.contains(".composer:focus-within .composer-more"));
}

#[tokio::test]
async fn the_job_page_shows_the_images_next_to_the_request() {
    let dashboard = dashboard();
    let id = job_id(
        &dashboard
            .post_job("maestro", "Match this", &[("a.png", PNG), ("b.jpg", JPEG)])
            .await,
    );
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let request = page.body.find("Match this").unwrap();
    let first = page
        .body
        .find(&format!("src=\"/jobs/{id}/images/image-1.png\""))
        .unwrap();
    let second = page
        .body
        .find(&format!("src=\"/jobs/{id}/images/image-2.jpg\""))
        .unwrap();
    assert!(request < first && first < second);
}

#[tokio::test]
async fn a_job_without_images_shows_no_image_list() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(!page.body.contains(r#"<ul class="request-images">"#));
}

#[tokio::test]
async fn a_stored_image_is_served_from_its_file() {
    let dashboard = dashboard();
    let id = job_id(
        &dashboard
            .post_job("maestro", "x", &[("a.png", PNG), ("b.webp", WEBP)])
            .await,
    );
    let png = dashboard
        .get(&format!("/jobs/{id}/images/image-1.png"))
        .await;
    assert_eq!(png.status, StatusCode::OK);
    assert_eq!(png.content_type.as_deref(), Some("image/png"));
    assert_eq!(png.bytes, PNG);
    let webp = dashboard
        .get(&format!("/jobs/{id}/images/image-2.webp"))
        .await;
    assert_eq!(webp.content_type.as_deref(), Some("image/webp"));
}

#[tokio::test]
async fn a_cleaned_merged_job_page_and_its_attachments_still_render() {
    let dashboard = dashboard();
    let id = job_id(
        &dashboard
            .post_job(
                "maestro",
                "Add a reboot button",
                &[("photo.png", PNG), ("notes.txt", TEXT)],
            )
            .await,
    );
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::Plan(plan_of_one())));
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 7 });
    dashboard.apply(id, Event::Merged);
    dashboard
        .store
        .append_factory_log(
            id,
            "Removed the checkout and session files to free disk space.",
        )
        .unwrap();

    // Create the checkout and session directories a real job keeps, then clean
    // them the way the job loop does once the job is merged.
    let dir = dashboard.data.path().join("jobs").join(id.to_string());
    std::fs::create_dir_all(dir.join("work")).unwrap();
    std::fs::create_dir_all(dir.join("claude")).unwrap();
    std::fs::create_dir_all(dir.join("codex")).unwrap();
    assert!(
        dashboard
            .store
            .clean_merged_job(id, dashboard.data.path())
            .unwrap()
    );
    assert!(!dir.join("work").exists());
    assert!(!dir.join("claude").exists());
    // Both agents' session directories go, so a Codex job frees its disk too.
    assert!(!dir.join("codex").exists());

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Add a reboot button"), "request");
    assert!(page.body.contains("Goal: a reboot button."), "spec");
    assert!(
        page.body.contains("maestrod: add a reboot endpoint"),
        "plan"
    );
    assert!(page.body.contains("free disk space"), "log");

    let image = dashboard
        .get(&format!("/jobs/{id}/images/image-1.png"))
        .await;
    assert_eq!(image.status, StatusCode::OK);
    assert_eq!(image.bytes, PNG);
    let file = dashboard.get(&format!("/jobs/{id}/files/file-1.txt")).await;
    assert_eq!(file.status, StatusCode::OK);
    assert_eq!(file.bytes, TEXT);
}

#[tokio::test]
async fn only_the_jobs_own_stored_images_are_served() {
    let dashboard = dashboard();
    let id = job_id(&dashboard.post_job("maestro", "x", &[("a.png", PNG)]).await);
    let other = job_id(&dashboard.post_job("maestro", "y", &[]).await);
    for uri in [
        format!("/jobs/{id}/images/image-2.png"),
        format!("/jobs/{other}/images/image-1.png"),
        format!("/jobs/{id}/images/..%2F..%2Fjobs.db"),
        format!("/jobs/{id}/images/image-1.txt"),
        "/jobs/999/images/image-1.png".to_owned(),
    ] {
        assert_eq!(
            dashboard.get(&uri).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
}

fn saved_files(dashboard: &Dashboard, id: i64) -> Vec<String> {
    let dir = dashboard
        .data
        .path()
        .join("jobs")
        .join(id.to_string())
        .join("files");
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn a_request_with_an_image_a_text_file_and_a_zip_saves_each_apart() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job(
            "maestro",
            "Fix this",
            &[("shot.png", PNG), ("server.log", TEXT), ("bundle.zip", ZIP)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let id = job_id(&reply);

    assert_eq!(dashboard.store.images(id).unwrap(), ["image-1.png"]);
    assert_eq!(saved_images(&dashboard, id), ["image-1.png"]);
    assert_eq!(saved_files(&dashboard, id), ["file-1.txt", "file-2.zip"]);
    let files = dashboard.store.files(id).unwrap();
    assert_eq!(files[0].name, "file-1.txt");
    assert_eq!(files[0].original_name, "server.log");
    assert_eq!(files[1].name, "file-2.zip");
    assert_eq!(files[1].original_name, "bundle.zip");
    let dir = dashboard
        .data
        .path()
        .join("jobs")
        .join(id.to_string())
        .join("files");
    assert_eq!(fs::read(dir.join("file-1.txt")).unwrap(), TEXT);
    assert_eq!(fs::read(dir.join("file-2.zip")).unwrap(), ZIP);
}

#[tokio::test]
async fn an_attachment_of_exactly_the_limit_is_accepted_and_one_more_byte_is_not() {
    let dashboard = dashboard();
    let mut exact = TEXT.to_vec();
    exact.resize(factory_server::MAX_ATTACHMENT_BYTES, b'x');
    let reply = dashboard
        .post_job("maestro", "x", &[("big.log", &exact)])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(dashboard.store.files(job_id(&reply)).unwrap().len(), 1);

    let mut over = exact;
    over.push(b'x');
    let reply = dashboard
        .post_job("maestro", "x", &[("big.log", &over)])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply
            .body
            .contains(&factory_server::MAX_ATTACHMENT_BYTES.to_string())
    );
}

#[tokio::test]
async fn attachments_without_text_are_rejected() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job("maestro", "  ", &[("a.log", TEXT)])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
    assert!(!dashboard.data.path().join("jobs").exists());
}

#[tokio::test]
async fn a_file_upload_name_is_cut_to_its_last_path_component() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job(
            "maestro",
            "x",
            &[("../../etc/passwd", TEXT), ("C:\\x\\y.txt", TEXT)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let id = job_id(&reply);
    let files = dashboard.store.files(id).unwrap();
    assert_eq!(files[0].original_name, "passwd");
    assert_eq!(files[1].original_name, "y.txt");
    // Only generated names reach the file system, both under files/.
    assert_eq!(saved_files(&dashboard, id), ["file-1.txt", "file-2.txt"]);
    let outside: Vec<_> = fs::read_dir(dashboard.data.path()).unwrap().collect();
    assert_eq!(outside.len(), 1, "only jobs/ exists in the data directory");
}

#[tokio::test]
async fn a_stored_file_is_served_as_an_attachment_with_its_original_name() {
    let dashboard = dashboard();
    let id = job_id(
        &dashboard
            .post_job("maestro", "x", &[("a b.log", TEXT), ("data.zip", ZIP)])
            .await,
    );
    let text = dashboard.get(&format!("/jobs/{id}/files/file-1.txt")).await;
    assert_eq!(text.status, StatusCode::OK);
    assert_eq!(
        text.content_type.as_deref(),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(text.bytes, TEXT);
    let disposition = text.header(header::CONTENT_DISPOSITION);
    assert_eq!(
        disposition.as_deref(),
        Some("attachment; filename*=UTF-8''a%20b.log")
    );
    assert_eq!(
        text.header(header::X_CONTENT_TYPE_OPTIONS).as_deref(),
        Some("nosniff")
    );

    let zip = dashboard.get(&format!("/jobs/{id}/files/file-2.zip")).await;
    assert_eq!(zip.content_type.as_deref(), Some("application/zip"));
    assert_eq!(zip.bytes, ZIP);
}

#[tokio::test]
async fn only_the_jobs_own_stored_files_are_served() {
    let dashboard = dashboard();
    let id = job_id(&dashboard.post_job("maestro", "x", &[("a.log", TEXT)]).await);
    // The other job also has a ZIP, so file-2.zip is a name it lists but this
    // job does not.
    let other = job_id(
        &dashboard
            .post_job("maestro", "y", &[("b.log", TEXT), ("c.zip", ZIP)])
            .await,
    );
    for uri in [
        // file-1.zip is not a name this job lists.
        format!("/jobs/{id}/files/file-1.zip"),
        format!("/jobs/{id}/files/file-2.txt"),
        // file-2.zip is listed for the other job, not this one.
        format!("/jobs/{id}/files/file-2.zip"),
        format!("/jobs/{id}/files/..%2F..%2Fjobs.db"),
        "/jobs/999/files/file-1.txt".to_owned(),
    ] {
        assert_eq!(
            dashboard.get(&uri).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    // Each job's own files still serve.
    assert_eq!(
        dashboard
            .get(&format!("/jobs/{id}/files/file-1.txt"))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        dashboard
            .get(&format!("/jobs/{other}/files/file-2.zip"))
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_job_page_lists_files_as_icon_tiles_with_escaped_names() {
    let dashboard = dashboard();
    let id = job_id(
        &dashboard
            .post_job(
                "maestro",
                "Look here",
                &[("x<b>&.log", TEXT), ("archive.zip", ZIP)],
            )
            .await,
    );
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains(&format!(r#"href="/jobs/{id}/files/file-1.txt""#))
    );
    assert!(
        page.body
            .contains(&format!(r#"href="/jobs/{id}/files/file-2.zip""#))
    );
    // The original name is HTML-escaped.
    assert!(page.body.contains("x&lt;b&gt;&amp;.log"));
    assert!(!page.body.contains("x<b>&.log"));
    // The tiles are inline SVG, fetched from no outside host.
    assert!(page.body.contains("<svg"));
}

#[tokio::test]
async fn a_job_without_files_shows_no_file_list() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(!page.body.contains(r#"<ul class="request-files">"#));
}

fn description() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "Adds POST /reboot for the device page.".into(),
    }
}

/// A job with the publish gate on whose checks passed, so it waits at the
/// publish approval for the factory to collect the summary.
async fn at_publish_gate(dashboard: &Dashboard) -> i64 {
    dashboard
        .store
        .save_project_gates(
            "maestro",
            Gates {
                spec: false,
                plan: false,
                publish: true,
            },
        )
        .unwrap();
    let id = dashboard.new_job().await;
    to_spec_approval(dashboard, id);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![PlannedPr {
            title: "maestrod: add a reboot endpoint".into(),
            summary: "POST /reboot".into(),
            number: None,
            description: None,
            notes: None,
        }])),
    );
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(description())));
    dashboard.apply(id, Event::ChecksPassed);
    id
}

fn branch_summary() -> BranchSummary {
    BranchSummary {
        commits: vec![
            "abc1234 maestrod: add the endpoint".into(),
            "def5678 maestrod: test the endpoint".into(),
        ],
        diff_stat: " src/main.rs | 4 ++++\n 1 file changed, 4 insertions(+)".into(),
    }
}

#[tokio::test]
async fn the_publish_approval_shows_the_pull_request_and_counts_as_waiting_for_you() {
    let dashboard = dashboard();
    let id = at_publish_gate(&dashboard).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(!page.body.contains("Approve and push"));
    assert!(page.body.contains("collecting the commits"));
    let index = dashboard.get("/").await;
    assert!(!index.body.contains("class=\"inbox inbox-you\""));

    dashboard.apply(id, Event::Summarized(branch_summary()));
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    for text in [
        "Approve before the first push",
        "maestrod: add a reboot endpoint",
        "Adds POST /reboot for the device page.",
        "abc1234 maestrod: add the endpoint",
        "def5678 maestrod: test the endpoint",
        "src/main.rs | 4 ++++",
        "1 file changed, 4 insertions(+)",
        "Approve and push",
        "Request changes",
    ] {
        assert!(page.body.contains(text), "{text}");
    }
    assert!(page.body.contains(r#"<pre class="doc diff-stat">"#));
    assert!(page.body.contains(&format!("/jobs/{id}/approve")));
    assert!(page.body.contains(&format!("/jobs/{id}/changes")));
    let index = dashboard.get("/").await;
    assert!(index.body.contains("class=\"inbox inbox-you\""));
    assert!(index.body.contains("needs you"));
}

#[tokio::test]
async fn approving_the_publish_gate_lets_the_factory_push() {
    let dashboard = dashboard();
    let id = at_publish_gate(&dashboard).await;
    dashboard.apply(id, Event::Summarized(branch_summary()));

    let reply = dashboard.post(&format!("/jobs/{id}/approve"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::PrOpen { pr: 0 }
    );
}

#[tokio::test]
async fn requesting_changes_at_the_publish_gate_sends_the_feedback_to_the_agent() {
    let dashboard = dashboard();
    let id = at_publish_gate(&dashboard).await;
    dashboard.apply(id, Event::Summarized(branch_summary()));

    let reply = dashboard
        .post(
            &format!("/jobs/{id}/changes"),
            "feedback=Squash+the+commits",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.turn(id),
        Turn::Agent(Task::Revise {
            feedback: "Squash the commits".into()
        })
    );
}

/// The text of the board section `class` on the index page, up to the next
/// section.
fn lane_html<'a>(body: &'a str, class: &str) -> &'a str {
    let marker = format!("class=\"inbox {class}\"");
    let start = body.find(&marker).unwrap_or_else(|| panic!("no {class}"));
    let rest = &body[start + marker.len()..];
    &rest[..rest.find("</section>").unwrap_or(rest.len())]
}

/// The board sections that list job `id`. A section renders only when it has
/// a job, so a class the board left out is skipped before `lane_html` reads
/// it.
fn lanes_of(body: &str, id: i64) -> Vec<&'static str> {
    let link = format!("href=\"/jobs/{id}\"");
    ["inbox-you", "inbox-failed", "inbox-running", "inbox-done"]
        .into_iter()
        .filter(|class| body.contains(&format!("class=\"inbox {class}\"")))
        .filter(|class| lane_html(body, class).contains(&link))
        .collect()
}

async fn assert_lane(dashboard: &Dashboard, id: i64, lane: &str) {
    let body = dashboard.get("/").await.body;
    assert_eq!(lanes_of(&body, id), [lane], "job {id}");
}

fn plan_of_one() -> Vec<PlannedPr> {
    vec![planned("maestrod: add a reboot endpoint", None)]
}

#[tokio::test]
async fn every_stage_and_turn_lands_in_exactly_one_lane() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    assert_lane(&dashboard, id, "inbox-running").await;

    let steps = [
        (Event::Start, "inbox-running"),
        (
            Event::Agent(AgentResult::Questions(vec![Question {
                text: "Which?".into(),
            }])),
            "inbox-you",
        ),
        (Event::Answer(vec!["This".into()]), "inbox-running"),
        (Event::Agent(AgentResult::Clarified), "inbox-running"),
        (Event::Agent(AgentResult::Spec("Goal.".into())), "inbox-you"),
        (Event::Approve, "inbox-running"),
        (Event::Agent(AgentResult::Plan(plan_of_one())), "inbox-you"),
        (Event::Approve, "inbox-running"),
        (
            Event::Agent(AgentResult::CommitsReady(ready())),
            "inbox-running",
        ),
        (Event::ChecksPassed, "inbox-running"),
        (Event::Published { number: 7 }, "inbox-you"),
        (
            Event::ReviewComments {
                comments: "Rename it".into(),
            },
            "inbox-running",
        ),
        (
            Event::Agent(AgentResult::CommitsReady(ready())),
            "inbox-running",
        ),
        (Event::ChecksPassed, "inbox-running"),
        (Event::Published { number: 7 }, "inbox-you"),
        (Event::Merged, "inbox-done"),
    ];
    for (event, lane) in steps {
        dashboard.apply(id, event.clone());
        let stage = dashboard.store.get(id).unwrap().job.stage().clone();
        let body = dashboard.get("/").await.body;
        assert_eq!(lanes_of(&body, id), [lane], "after {event:?} in {stage:?}");
    }
}

#[tokio::test]
async fn a_failed_job_lands_in_the_failed_lane_and_cancelled_in_done() {
    let dashboard = dashboard();
    let failed = dashboard.new_job().await;
    dashboard.apply(failed, Event::Start);
    dashboard.apply(
        failed,
        Event::AgentFailed {
            reason: "boom".into(),
        },
    );
    let cancelled = dashboard.new_job().await;
    dashboard.apply(cancelled, Event::Cancel);
    assert_lane(&dashboard, failed, "inbox-failed").await;
    assert_lane(&dashboard, cancelled, "inbox-done").await;
}

#[tokio::test]
async fn the_publish_gate_waits_in_factory_then_you() {
    let dashboard = dashboard();
    let id = at_publish_gate(&dashboard).await;
    assert_lane(&dashboard, id, "inbox-running").await;
    dashboard.apply(id, Event::Summarized(branch_summary()));
    assert_lane(&dashboard, id, "inbox-you").await;
}

#[tokio::test]
async fn a_pull_request_in_review_moves_to_factory_with_auto_merge() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::Plan(plan_of_one())));
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 3 });
    assert_lane(&dashboard, id, "inbox-you").await;
    dashboard
        .store
        .save_project_auto_merge("maestro", true)
        .unwrap();
    assert_lane(&dashboard, id, "inbox-running").await;
}

#[tokio::test]
async fn the_board_replaces_the_two_lists() {
    let body = dashboard().get("/").await.body;
    assert!(!body.contains("Waiting for you"));
    assert!(!body.contains("All jobs"));
    assert!(!body.contains("No jobs yet."));
    assert!(!body.contains("Live jobs by lane"));
}

#[tokio::test]
async fn an_empty_board_hides_every_section_and_says_it_is_quiet() {
    let body = dashboard().get("/").await.body;
    assert!(!body.contains("class=\"inbox "));
    assert!(body.contains("Nothing needs you and no job is running."));

    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let body = dashboard.get("/").await.body;
    assert_eq!(lanes_of(&body, id), ["inbox-running"]);
    assert!(!body.contains("Nothing needs you and no job is running."));
    assert!(!body.contains("inbox-you"));
    assert!(!body.contains("inbox-done"));

    dashboard.apply(id, Event::Cancel);
    let body = dashboard.get("/").await.body;
    assert_eq!(lanes_of(&body, id), ["inbox-done"]);
    assert!(body.contains("Nothing needs you and no job is running."));
}

#[tokio::test]
async fn sections_show_their_job_counts_in_the_order_you_act() {
    let dashboard = dashboard();
    let a = dashboard.new_job().await;
    let b = dashboard.new_job().await;
    let c = dashboard.new_job().await;
    dashboard.new_job().await;
    to_spec_approval(&dashboard, a);
    to_spec_approval(&dashboard, b);
    dashboard.apply(c, Event::Cancel);
    failed_out_of_rounds(&dashboard).await;
    let body = dashboard.get("/").await.body;
    assert!(body.contains("Needs you <span class=\"count\">2</span>"));
    assert!(body.contains("Failed <span class=\"count\">1</span>"));
    assert!(body.contains("Running <span class=\"count\">1</span>"));
    assert!(body.contains("Done <span class=\"count\">1</span>"));
    let at = |class: &str| body.find(&format!("class=\"inbox {class}\"")).unwrap();
    assert!(at("inbox-you") < at("inbox-failed"));
    assert!(at("inbox-failed") < at("inbox-running"));
    assert!(at("inbox-running") < at("inbox-done"));
}

#[tokio::test]
async fn done_shows_five_jobs_and_collapses_the_older_ones() {
    let dashboard = dashboard();
    let mut ids = Vec::new();
    for _ in 0..7 {
        let id = dashboard.new_job().await;
        dashboard.apply(id, Event::Cancel);
        ids.push(id);
    }
    let body = dashboard.get("/").await.body;
    let done = lane_html(&body, "inbox-done");
    assert!(done.contains("Done <span class=\"count\">7</span>"));
    // The disclosure carries a stable key, so `details-state.js` can reopen it
    // across a poll swap.
    let open = done
        .find("<details class=\"older\" data-key=\"done\">")
        .expect("details.older carries its key");
    let older = &done[open..];
    assert!(older.contains("<summary>Show 2 older</summary>"));
    // Newest first, so the two oldest jobs are the collapsed ones.
    for id in &ids[..2] {
        assert!(older.contains(&format!("href=\"/jobs/{id}\"")), "{id}");
    }
    for id in &ids[2..] {
        assert!(!older.contains(&format!("href=\"/jobs/{id}\"")), "{id}");
    }
    // The server renders it closed; the script opens it only in the browser.
    assert!(!done.contains("<details class=\"older\" data-key=\"done\" open"));
}

#[tokio::test]
async fn the_board_and_job_page_serve_the_details_state_script() {
    // The script keeps an open <details> open across a poll swap. It is inlined
    // once per page, outside every swapped region.
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    for uri in ["/", &format!("/jobs/{id}")] {
        let body = dashboard.get(uri).await.body;
        assert!(
            body.contains("details[data-key]"),
            "{uri} does not inline details-state.js"
        );
    }
}

#[tokio::test]
async fn done_has_no_collapsed_part_with_five_jobs_or_fewer() {
    let dashboard = dashboard();
    for _ in 0..5 {
        let id = dashboard.new_job().await;
        dashboard.apply(id, Event::Cancel);
    }
    let body = dashboard.get("/").await.body;
    assert!(!body.contains("class=\"older\""));
}

#[tokio::test]
async fn each_section_lists_the_newest_job_first() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "inbox-running");
    let older = lane.find(&format!("href=\"/jobs/{first}\"")).unwrap();
    let newer = lane.find(&format!("href=\"/jobs/{second}\"")).unwrap();
    assert!(newer < older);
}

#[tokio::test]
async fn rows_show_the_id_title_badge_stage_and_requester() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "inbox-running");
    assert!(lane.contains(&format!("#{id}")));
    assert!(lane.contains("Add a reboot button</a>"));
    assert!(lane.contains("class=\"badge factory\">queued"));
    assert!(lane.contains("<span class=\"stage\">requested</span>"));
    let at = |needle: &str| lane.find(needle).unwrap();
    assert!(at("class=\"id\"") < at("class=\"title\""));
    assert!(at("class=\"title\"") < at("class=\"badge factory\""));
    assert!(at("class=\"badge factory\"") < at("class=\"stage\""));
    assert!(at("class=\"stage\"") < at("class=\"requester\""));
    assert!(at("class=\"requester\"") < at("class=\"cost\""));
    assert!(!lane.contains("maestro"));
    assert!(lane.contains("alice@example.com"));
}

#[tokio::test]
async fn a_live_row_shows_its_place_in_the_job_flow() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let body = dashboard.get("/").await.body;
    assert!(body.contains("aria-label=\"Step 1 of 9: requested\""));

    to_spec_approval(&dashboard, id);
    let body = dashboard.get("/").await.body;
    let row = lane_html(&body, "inbox-you");
    assert!(row.contains("aria-label=\"Step 3 of 9: spec\""));
    assert_eq!(row.matches("class=\"step done\"").count(), 2);
    assert_eq!(row.matches("class=\"step current\"").count(), 1);
    assert_eq!(row.matches("class=\"step\"").count(), 6);
}

#[tokio::test]
async fn finished_rows_show_no_flow_and_no_repeated_stage() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Cancel);
    let body = dashboard.get("/").await.body;
    let done = lane_html(&body, "inbox-done");
    assert!(!done.contains("class=\"steps\""));
    assert!(!done.contains("class=\"stage\""));
    assert!(done.contains("class=\"badge factory\">cancelled"));
}

#[tokio::test]
async fn a_failed_row_says_why_it_failed() {
    let dashboard = dashboard();
    failed_out_of_rounds(&dashboard).await;
    let body = dashboard.get("/").await.body;
    let failed = lane_html(&body, "inbox-failed");
    assert!(
        failed.contains(
            "<span class=\"stage\">5 failed check or CI rounds on one pull request</span>"
        )
    );
    assert!(!failed.contains("class=\"steps\""));
}

#[tokio::test]
async fn a_gate_row_links_to_what_you_approve() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    let body = dashboard.get("/").await.body;
    let row = lane_html(&body, "inbox-you");
    assert!(row.contains(&format!(
        "<a class=\"link-button\" href=\"/jobs/{id}\">Read spec</a>"
    )));
    assert!(row.contains("Approve spec"));
}

#[tokio::test]
async fn a_row_waiting_on_questions_links_to_the_job_page() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Questions(vec![Question {
            text: "Which board?".into(),
        }])),
    );
    let body = dashboard.get("/").await.body;
    let row = lane_html(&body, "inbox-you");
    assert!(row.contains(&format!(
        "<a class=\"link-button\" href=\"/jobs/{id}\">Open</a>"
    )));
    assert!(!row.contains("<form"));
}

#[tokio::test]
async fn only_the_you_lane_has_forms_and_buttons() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    let body = dashboard.get("/").await.body;
    let board = &body[body.find("class=\"board\"").unwrap()..];
    assert_eq!(board.matches("<form").count(), 1);
    assert_eq!(board.matches("<button").count(), 1);
    assert!(lane_html(&body, "inbox-you").contains("<form"));
}

#[tokio::test]
async fn the_board_counts_only_the_selected_projects_jobs() {
    let dashboard = two_projects();
    dashboard.new_job().await;
    dashboard.post_job("tool", "Tool change", &[]).await;
    dashboard.post_job("tool", "Another tool change", &[]).await;
    let body = dashboard.get("/?project=tool").await.body;
    assert!(body.contains("Running <span class=\"count\">2</span>"));
    let body = dashboard.get("/?project=maestro").await.body;
    assert!(body.contains("Running <span class=\"count\">1</span>"));
    assert!(!body.contains("Tool change"));
}

fn plan_one() -> Event {
    Event::Agent(AgentResult::Plan(vec![planned(
        "maestrod: add a reboot endpoint",
        None,
    )]))
}

#[tokio::test]
async fn the_board_offers_approve_at_the_spec_gate() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);

    let index = dashboard.get("/").await;
    assert!(index.body.contains("Approve spec"));
    assert!(
        index
            .body
            .contains(&format!("action=\"/jobs/{id}/approve\""))
    );
}

#[tokio::test]
async fn the_board_offers_approve_at_the_plan_gate() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, plan_one());

    let index = dashboard.get("/").await;
    assert!(index.body.contains("Approve plan"));
    assert!(!index.body.contains("Approve spec"));
}

#[tokio::test]
async fn the_board_offers_no_approve_for_questions_review_or_other_lanes() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    assert!(!dashboard.get("/").await.body.contains("Approve"));

    dashboard.apply(id, Event::Start);
    assert!(!dashboard.get("/").await.body.contains("Approve"));

    dashboard.apply(
        id,
        Event::Agent(AgentResult::Questions(vec![Question {
            text: "Which board?".into(),
        }])),
    );
    assert!(!dashboard.get("/").await.body.contains("Approve"));
    dashboard.apply(id, Event::Answer(vec!["one".into()]));
    dashboard.apply(id, Event::Agent(AgentResult::Clarified));
    assert!(!dashboard.get("/").await.body.contains("Approve"));

    dashboard.apply(id, Event::Agent(AgentResult::Spec("s".into())));
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, plan_one());
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 15 });
    let index = dashboard.get("/").await;
    assert!(index.body.contains("in review"));
    assert!(!index.body.contains("Approve"));
}

#[tokio::test]
async fn the_board_offers_no_approve_at_the_publish_gate() {
    let dashboard = dashboard();
    let id = at_publish_gate(&dashboard).await;
    dashboard.apply(id, Event::Summarized(branch_summary()));
    assert!(!dashboard.get("/").await.body.contains("Approve spec"));
    assert!(!dashboard.get("/").await.body.contains("Approve plan"));
}

#[tokio::test]
async fn approving_from_the_board_returns_to_the_selected_project() {
    let dashboard = two_projects();
    dashboard.post_job("tool", "Tool change", &[]).await;
    let id = 1;
    to_spec_approval(&dashboard, id);

    let index = dashboard.get("/?project=tool").await;
    assert!(index.body.contains("name=\"project\" value=\"tool\""));
    let reply = dashboard
        .post(&format!("/jobs/{id}/approve"), "project=tool")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/?project=tool"));
    assert_eq!(dashboard.store.get(id).unwrap().job.stage(), &Stage::Plan);
}

#[tokio::test]
async fn approving_from_the_job_page_still_redirects_to_the_job() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    let reply = dashboard.post(&format!("/jobs/{id}/approve"), "").await;
    assert_eq!(reply.location, Some(format!("/jobs/{id}")));

    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    let reply = dashboard
        .post(&format!("/jobs/{id}/approve"), "project=nope")
        .await;
    assert_eq!(reply.location, Some(format!("/jobs/{id}")));
}

fn merge(dashboard: &Dashboard, id: i64) {
    to_spec_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![planned("maestrod: reboot", None)])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 15 });
    dashboard.apply(id, Event::Merged);
    assert_eq!(dashboard.store.get(id).unwrap().job.stage(), &Stage::Merged);
}

fn tile(value: &str, label: &str) -> String {
    format!("<div class=\"total\"><dt>{label}</dt><dd>{value}</dd></div>")
}

#[tokio::test]
async fn the_stats_strip_shows_the_stored_totals() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    dashboard.new_job().await;
    dashboard.new_job().await;
    merge(&dashboard, first);
    merge(&dashboard, second);
    dashboard.store.add_cost(first, 1.0, false).unwrap();
    dashboard.store.add_cost(second, 2.5, false).unwrap();

    let body = dashboard.get("/").await.body;
    assert!(body.contains(&tile("$3.50", "Total API cost")));
    assert!(body.contains(&tile("4", "Jobs created")));
    assert!(body.contains(&tile("2", "Jobs merged")));
    assert!(body.contains(&tile("50%", "Merge rate")));
    assert!(body.contains(&tile("$1.75", "Cost per merged job")));
}

#[tokio::test]
async fn the_stats_strip_shows_dashes_for_a_project_without_jobs() {
    let body = dashboard().get("/").await.body;
    assert!(body.contains(&tile("$0.00", "Total API cost")));
    assert!(body.contains(&tile("–", "Jobs created")));
    assert!(body.contains(&tile("–", "Jobs merged")));
    assert!(body.contains(&tile("–", "Merge rate")));
    assert!(body.contains(&tile("–", "Cost per merged job")));
    assert!(!body.contains("NaN"));
    assert!(!body.contains("inf"));
}

#[tokio::test]
async fn jobs_without_a_merge_show_dashes_for_the_ratios() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 2.0, false).unwrap();

    let body = dashboard.get("/").await.body;
    assert!(body.contains(&tile("$2.00", "Total API cost")));
    assert!(body.contains(&tile("1", "Jobs created")));
    assert!(body.contains(&tile("–", "Jobs merged")));
    assert!(body.contains(&tile("0%", "Merge rate")));
    assert!(body.contains(&tile("–", "Cost per merged job")));
    assert!(!body.contains("NaN"));
}

#[tokio::test]
async fn the_stats_strip_follows_the_selected_project() {
    let dashboard = two_projects();
    dashboard.new_job().await;

    let tool = dashboard.get("/?project=tool").await.body;
    assert!(tool.contains(&tile("–", "Jobs created")));
    let maestro = dashboard.get("/?project=maestro").await.body;
    assert!(maestro.contains(&tile("1", "Jobs created")));
}

#[tokio::test]
async fn a_board_card_shows_the_job_cost() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 0.5, false).unwrap();
    dashboard.store.add_cost(id, 0.75, false).unwrap();

    let index = dashboard.get("/").await;
    assert!(index.body.contains("<span class=\"cost\">$1.25</span>"));
}

#[tokio::test]
async fn a_board_card_without_cost_shows_a_dash() {
    let dashboard = dashboard();
    dashboard.new_job().await;

    let index = dashboard.get("/").await;
    assert!(index.body.contains("<span class=\"cost\">–</span>"));
}

#[tokio::test]
async fn a_board_card_does_not_repeat_the_project_name() {
    let dashboard = dashboard();
    dashboard.new_job().await;

    let index = dashboard.get("/").await;
    assert!(!index.body.contains("maestro · "));
}

#[tokio::test]
async fn the_approve_button_on_a_card_still_posts() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.store.add_cost(id, 1.0, false).unwrap();

    let index = dashboard.get("/").await;
    assert!(index.body.contains("<span class=\"cost\">$1.00</span>"));
    assert!(index.body.contains("method=\"post\""));
    assert!(index.body.contains("Approve spec"));

    let reply = dashboard
        .post(&format!("/jobs/{id}/approve"), "project=maestro")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(matches!(dashboard.turn(id), Turn::Agent(_)));
}

#[test]
fn merge_fraction_is_the_merged_share_of_created() {
    assert!((factory_server::merge_fraction(4, 1) - 0.25).abs() < f64::EPSILON);
    assert!((factory_server::merge_fraction(3, 3) - 1.0).abs() < f64::EPSILON);
    assert!(factory_server::merge_fraction(5, 0).abs() < f64::EPSILON);
}

#[test]
fn merge_fraction_is_zero_without_created_jobs() {
    let fraction = factory_server::merge_fraction(0, 0);
    assert!(!fraction.is_nan());
    assert!(fraction.abs() < f64::EPSILON);
    assert!(factory_server::merge_fraction(0, 2).abs() < f64::EPSILON);
}

fn models(clarify: Option<&str>, implement: Option<&str>) -> StageModels {
    StageModels {
        clarify: clarify.map(str::to_owned),
        spec: None,
        plan: None,
        implement: implement.map(str::to_owned),
        review: None,
    }
}

#[test]
fn project_models_are_stored_and_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let none = StageModels::default();
    let saved = models(Some("opus"), Some("sonnet"));
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.project_models("maestro", &none).unwrap(), none);
        store.save_project_models("maestro", &saved).unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.project_models("maestro", &none).unwrap(), saved);
    assert_eq!(store.project_models("other", &none).unwrap(), none);
}

#[test]
fn the_review_model_is_stored_and_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let none = StageModels::default();
    let saved = StageModels {
        review: Some("opus".into()),
        ..StageModels::default()
    };
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.project_models("maestro", &none).unwrap().review, None);
        store.save_project_models("maestro", &saved).unwrap();
    }
    // The review model survives a reopen alongside the other stages.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.project_models("maestro", &none).unwrap(), saved);
    // Clearing it saves an empty review again.
    store.save_project_models("maestro", &none).unwrap();
    assert_eq!(store.project_models("maestro", &saved).unwrap(), none);
}

#[test]
fn an_old_project_models_table_gains_the_review_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    // A `project_models` table as it was before `review` joined the stages,
    // with a saved row for one project.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE project_models (
                project TEXT PRIMARY KEY,
                clarify TEXT,
                spec TEXT,
                plan TEXT,
                implement TEXT
            );
            INSERT INTO project_models (project, clarify) VALUES ('maestro', 'opus');",
        )
        .unwrap();
    }
    // Opening the store adds the column, so the old row reads with no review
    // and a new review can be saved.
    let store = Store::open(&path).unwrap();
    let none = StageModels::default();
    let reloaded = store.project_models("maestro", &none).unwrap();
    assert_eq!(reloaded.clarify.as_deref(), Some("opus"));
    assert_eq!(reloaded.review, None);
    let with_review = StageModels {
        clarify: Some("opus".into()),
        review: Some("sonnet".into()),
        ..StageModels::default()
    };
    store.save_project_models("maestro", &with_review).unwrap();
    assert_eq!(store.project_models("maestro", &none).unwrap(), with_review);
}

fn efforts(clarify: &str, spec: &str, plan: &str, implement: &str, review: &str) -> StageEfforts {
    StageEfforts {
        clarify: clarify.into(),
        spec: spec.into(),
        plan: plan.into(),
        implement: implement.into(),
        review: review.into(),
    }
}

#[test]
fn a_project_without_effort_reads_medium_for_every_stage() {
    let store = Store::open_in_memory().unwrap();
    let medium = StageEfforts::default();
    assert_eq!(medium.clarify, DEFAULT_AGENT.default_effort());
    assert_eq!(medium.review, DEFAULT_AGENT.default_effort());
    assert_eq!(store.project_efforts("maestro").unwrap(), medium);
}

#[test]
fn project_efforts_are_stored_and_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let saved = efforts("low", "medium", "high", "xhigh", "max");
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.project_efforts("maestro").unwrap(),
            StageEfforts::default()
        );
        store
            .save_project_efforts("maestro", &saved, &StageAgents::default())
            .unwrap();
    }
    // Every stage's effort survives a reopen, and another project is untouched.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.project_efforts("maestro").unwrap(), saved);
    assert_eq!(
        store.project_efforts("other").unwrap(),
        StageEfforts::default()
    );
}

#[test]
fn saving_one_projects_effort_leaves_other_projects_alone() {
    let store = Store::open_in_memory().unwrap();
    store
        .save_project_efforts(
            "maestro",
            &efforts("high", "high", "high", "high", "high"),
            &StageAgents::default(),
        )
        .unwrap();
    store
        .save_project_efforts(
            "factory",
            &efforts("low", "low", "low", "low", "low"),
            &StageAgents::default(),
        )
        .unwrap();
    store
        .save_project_efforts(
            "maestro",
            &efforts("max", "max", "max", "max", "max"),
            &StageAgents::default(),
        )
        .unwrap();
    assert_eq!(
        store.project_efforts("factory").unwrap(),
        efforts("low", "low", "low", "low", "low")
    );
    assert_eq!(
        store.project_efforts("maestro").unwrap(),
        efforts("max", "max", "max", "max", "max")
    );
}

#[test]
fn an_effort_outside_the_allowed_set_is_rejected_and_not_stored() {
    let store = Store::open_in_memory().unwrap();
    let bad = efforts("medium", "medium", "turbo", "medium", "medium");
    let err = store
        .save_project_efforts("maestro", &bad, &StageAgents::default())
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidEffort(level) if level == "turbo"),
        "{err:?}"
    );
    // Nothing was written, so the project still reads the default.
    assert_eq!(
        store.project_efforts("maestro").unwrap(),
        StageEfforts::default()
    );
}

#[test]
fn an_effort_is_validated_against_its_stages_saved_agent() {
    let store = Store::open_in_memory().unwrap();
    // Codex's levels run minimal through xhigh, so xhigh is valid for a codex
    // stage even though a claude stage would reject it below max.
    let codex = agents("codex", "codex", "codex", "codex", "codex");
    store
        .save_project_efforts(
            "maestro",
            &efforts("minimal", "low", "medium", "high", "xhigh"),
            &codex,
        )
        .unwrap();
    assert_eq!(
        store.project_efforts("maestro").unwrap(),
        efforts("minimal", "low", "medium", "high", "xhigh")
    );

    // `max` is Claude's top level and no level of Codex, so a codex stage
    // rejects it and writes nothing.
    let err = store
        .save_project_efforts(
            "maestro",
            &efforts("max", "low", "medium", "high", "xhigh"),
            &codex,
        )
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidEffort(level) if level == "max"),
        "{err:?}"
    );

    // `minimal` is a Codex level and no level of Claude, so a claude stage
    // rejects it.
    let err = store
        .save_project_efforts(
            "maestro",
            &efforts("minimal", "low", "low", "low", "low"),
            &StageAgents::default(),
        )
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidEffort(level) if level == "minimal"),
        "{err:?}"
    );

    // The earlier codex save is the only one that wrote.
    assert_eq!(
        store.project_efforts("maestro").unwrap(),
        efforts("minimal", "low", "medium", "high", "xhigh")
    );
}

#[test]
fn a_database_from_before_the_effort_table_opens_and_reads_medium() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    // A database as it was before `project_efforts` existed: it has a project
    // but no effort table at all.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                name TEXT PRIMARY KEY,
                repo TEXT NOT NULL UNIQUE,
                base TEXT NOT NULL,
                checks TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO projects (name, repo, base) VALUES ('maestro', 'o/n', 'main');",
        )
        .unwrap();
    }
    // Opening the store creates the table, so the old project reads medium and
    // a new effort can be saved.
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.project_efforts("maestro").unwrap(),
        StageEfforts::default()
    );
    let saved = efforts("high", "high", "high", "high", "high");
    store
        .save_project_efforts("maestro", &saved, &StageAgents::default())
        .unwrap();
    assert_eq!(store.project_efforts("maestro").unwrap(), saved);
}

#[tokio::test]
async fn the_settings_form_has_an_effort_selector_per_stage_preselected_to_the_effective_value() {
    let dashboard = dashboard();
    let page = dashboard.get("/settings/projects/maestro").await;
    // Each stage's panel has a radio group named "<stage>_effort".
    for stage in ["clarify", "spec", "plan", "implement", "review"] {
        assert!(
            page.body.contains(&format!("name=\"{stage}_effort\"")),
            "missing the {stage} effort selector",
        );
    }
    // The selector offers only the allowed levels.
    for level in ["low", "medium", "high", "xhigh", "max"] {
        assert!(page.body.contains(&format!("value=\"{level}\"")));
    }
    // With nothing saved, every stage preselects the medium default, so the
    // selected option appears once per stage.
    let doc = scraper::Html::parse_document(&page.body);
    let medium =
        scraper::Selector::parse(".pipe-panel input[name$=_effort][value=medium][checked]")
            .unwrap();
    assert_eq!(doc.select(&medium).count(), 5);

    // A saved level preselects in place of the default.
    dashboard
        .store
        .save_project_efforts(
            "maestro",
            &efforts("low", "medium", "high", "xhigh", "max"),
            &StageAgents::default(),
        )
        .unwrap();
    let page = dashboard.get("/settings/projects/maestro").await;
    assert!(page.body.contains("value=\"low\" checked"));
    assert!(page.body.contains("value=\"high\" checked"));
    assert!(page.body.contains("value=\"xhigh\" checked"));
    assert!(page.body.contains("value=\"max\" checked"));
}

#[tokio::test]
async fn the_pipeline_form_carries_each_agents_effort_levels_for_the_panel_script() {
    let dashboard = dashboard();
    let page = dashboard.get("/settings/projects/maestro").await;
    // The form holds an agent-to-efforts map the pipeline script reads to redraw
    // the effort radios when a stage's agent switches. It names both agents with
    // their own levels, so a switch to Codex can show minimal and drop max. The
    // attribute is HTML-escaped, so its quotes read as &quot; in the source.
    assert!(
        page.body.contains("data-agent-efforts="),
        "the form carries no agent effort map",
    );
    for level in ["minimal", "low", "medium", "high", "xhigh"] {
        assert!(
            page.body.contains(&format!("&quot;{level}&quot;")),
            "the agent effort map is missing {level}",
        );
    }
    assert!(
        page.body.contains("&quot;codex&quot;") && page.body.contains("&quot;claude&quot;"),
        "the agent effort map names both agents",
    );
}

#[tokio::test]
async fn each_stage_panel_offers_its_saved_agents_models_and_cli_hint() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_agents(
            "maestro",
            &agents("claude", "codex", "copilot", "copilot", "claude"),
        )
        .unwrap();
    let page = dashboard.get("/settings/projects/maestro").await;
    let panel = |stage: &str| {
        let start = page.body.find(&format!("data-panel=\"{stage}\"")).unwrap();
        let rest = &page.body[start..];
        let end = rest.find("data-model-hint").unwrap();
        let hint = rest[end..].find("</p>").unwrap();
        rest[..end + hint].to_owned()
    };
    let chips = |html: &str| -> Vec<String> {
        html.split("data-model-for=")
            .skip(1)
            .map(|part| {
                part.split('>')
                    .nth(1)
                    .unwrap()
                    .split('<')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };

    let claude = panel("clarify");
    assert_eq!(
        chips(&claude),
        ["CLI default", "fable", "opus", "sonnet", "haiku"]
    );
    assert!(claude.contains("installed claude CLI picks"));
    assert!(claude.contains("<datalist id=\"model-suggestions-clarify\">"));

    let codex = panel("spec");
    assert_eq!(
        chips(&codex),
        ["CLI default", "gpt-6.1-sol", "gpt-6.1-codex"]
    );
    assert!(codex.contains("installed codex CLI picks"));

    let copilot = panel("implement");
    assert_eq!(
        chips(&copilot),
        [
            "CLI default",
            "claude-sonnet-5.5",
            "claude-opus-5.5",
            "gpt-6.2-sol",
            "gemini-3.8-flash"
        ],
    );
    assert!(copilot.contains("installed copilot CLI picks"));
    assert!(copilot.contains("<option value=\"gemini-3.8-flash\">"));
    for alias in ["fable", "opus", "sonnet", "haiku"] {
        assert!(
            !copilot.contains(&format!("value=\"{alias}\"")),
            "a Copilot stage offers Claude's {alias}",
        );
    }
    assert!(!page.body.contains("model-aliases"));
    for agent in ["claude", "codex", "copilot"] {
        assert!(page.body.contains(&format!("&quot;{agent}&quot;")));
    }
    assert!(page.body.contains("data-agent-models="));
}

#[tokio::test]
async fn a_local_stage_offers_the_stored_models_and_names_the_server() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_local_settings(Some("http://gfx1201:9931"), factory_core::Cli::Codex, 10)
        .unwrap();
    dashboard
        .store
        .replace_local_models(&["gemma-4-26B".to_owned(), "Llama-3.2-1B".to_owned()], 1)
        .unwrap();
    dashboard
        .store
        .save_project_agents(
            "maestro",
            &agents("local", "claude", "claude", "claude", "claude"),
        )
        .unwrap();
    let page = dashboard.get("/settings/projects/maestro").await;
    let start = page.body.find("data-panel=\"clarify\"").unwrap();
    let rest = &page.body[start..];
    let panel = &rest[..rest.find("data-model-hint").unwrap() + 200];
    assert!(panel.contains("data-model=\"gemma-4-26B\""));
    assert!(panel.contains("data-model=\"Llama-3.2-1B\""));
    assert!(!panel.contains("data-model=\"\""), "no CLI default chip");
    assert!(panel.contains("<option value=\"Llama-3.2-1B\">"));
    assert!(panel.contains("Models served by http://gfx1201:9931."));
    assert!(page.body.contains("&quot;local&quot;"));
}

#[tokio::test]
async fn a_local_stage_without_a_stored_list_offers_no_chips() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_local_settings(Some("http://gfx1201:9931"), factory_core::Cli::Codex, 10)
        .unwrap();
    dashboard
        .store
        .save_project_agents(
            "maestro",
            &agents("local", "claude", "claude", "claude", "claude"),
        )
        .unwrap();
    let page = dashboard.get("/settings/projects/maestro").await;
    let start = page.body.find("data-panel=\"clarify\"").unwrap();
    let rest = &page.body[start..];
    let panel = &rest[..rest.find("data-model-hint").unwrap() + 200];
    assert!(!panel.contains("data-model-for="));
    assert!(!panel.contains("<option"));
    assert!(panel.contains("No model list is stored for http://gfx1201:9931."));
}

#[tokio::test]
async fn a_list_stored_for_another_server_is_not_labelled_as_the_current_one() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_local_settings(Some("http://old:1"), factory_core::Cli::Codex, 10)
        .unwrap();
    dashboard
        .store
        .replace_local_models(&["gemma-4-26B".to_owned()], 1)
        .unwrap();
    dashboard
        .store
        .save_local_settings(Some("http://new:2"), factory_core::Cli::Codex, 10)
        .unwrap();
    dashboard
        .store
        .save_project_agents(
            "maestro",
            &agents("local", "claude", "claude", "claude", "claude"),
        )
        .unwrap();
    let page = dashboard.get("/settings/projects/maestro").await;
    assert!(page.body.contains("data-model=\"gemma-4-26B\""));
    assert!(
        page.body
            .contains("Models from http://old:1, the server before http://new:2.")
    );
    assert!(!page.body.contains("Models served by http://new:2"));
}

#[test]
fn a_list_stored_before_models_url_has_no_known_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE local_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1), url TEXT,
                cli TEXT NOT NULL DEFAULT 'codex',
                load_timeout_minutes INTEGER NOT NULL DEFAULT 10,
                models_fetched_at INTEGER,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             INSERT INTO local_settings (id, url, models_fetched_at)
                VALUES (1, 'http://b:2', 5);",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let settings = store.local_settings().unwrap();
    assert_eq!(settings.url.as_deref(), Some("http://b:2"));
    assert_eq!(settings.models_url, None);
    store.replace_local_models(&["m".to_owned()], 6).unwrap();
    assert_eq!(
        store.local_settings().unwrap().models_url.as_deref(),
        Some("http://b:2")
    );
}

#[tokio::test]
async fn a_list_of_unknown_source_says_so_in_the_hint() {
    let dashboard = dashboard();
    dashboard
        .store
        .replace_local_models(&["gemma-4-26B".to_owned()], 1)
        .unwrap();
    dashboard
        .store
        .save_local_settings(Some("http://new:2"), factory_core::Cli::Codex, 10)
        .unwrap();
    dashboard
        .store
        .save_project_agents(
            "maestro",
            &agents("local", "claude", "claude", "claude", "claude"),
        )
        .unwrap();
    let page = dashboard.get("/settings/projects/maestro").await;
    assert!(page.body.contains("a server the factory did not record"));
    assert!(!page.body.contains("Models served by"));
}

#[tokio::test]
async fn a_blank_local_stage_reads_model_required_on_its_node() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_agents(
            "maestro",
            &agents("local", "claude", "claude", "claude", "claude"),
        )
        .unwrap();
    let page = dashboard.get("/settings/projects/maestro").await;
    assert!(
        page.body
            .contains("data-model-label=\"clarify\">Model required<")
    );
    assert!(page.body.contains("data-model-label=\"spec\">CLI default<"));
}

#[tokio::test]
async fn a_collaborator_who_is_not_an_admin_can_pick_local_in_every_stage() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    dashboard
        .store
        .add_collaborator("maestro", "bob@example.com", "test")
        .unwrap();
    let page = dashboard
        .get_as("bob@example.com", "/settings/projects/maestro")
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.body.matches("value=\"local\"").count(),
        6,
        "one Local radio per stage and one in the Renovate agent card",
    );
}

#[tokio::test]
async fn saving_the_form_persists_model_and_effort_and_leaves_other_projects_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    store
        .create_project(
            "tool",
            &ForgeKind::GitHub,
            "Badger-Embedded/tool",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with(Arc::clone(&store));

    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=opus&spec=sonnet&plan=haiku&implement=opus&review=sonnet&\
             clarify_effort=low&spec_effort=medium&plan_effort=high&\
             implement_effort=xhigh&review_effort=max",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);

    // Reopen the database: both the models and the efforts survived, as the
    // review model does.
    drop(dashboard);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .project_models("maestro", &StageModels::default())
            .unwrap(),
        StageModels {
            clarify: Some("opus".into()),
            spec: Some("sonnet".into()),
            plan: Some("haiku".into()),
            implement: Some("opus".into()),
            review: Some("sonnet".into()),
        }
    );
    assert_eq!(
        store.project_efforts("maestro").unwrap(),
        efforts("low", "medium", "high", "xhigh", "max")
    );
    // Saving maestro wrote no row for tool: it still reads every default.
    assert_eq!(
        store
            .project_models("tool", &StageModels::default())
            .unwrap(),
        StageModels::default()
    );
    assert_eq!(
        store.project_efforts("tool").unwrap(),
        StageEfforts::default()
    );
}

#[tokio::test]
async fn an_invalid_effort_submission_is_rejected_and_stores_nothing() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=opus&spec=&plan=&implement=&review=&\
             clarify_effort=medium&spec_effort=turbo&plan_effort=medium&\
             implement_effort=medium&review_effort=medium",
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    // Efforts are saved first, so the model submitted alongside the bad level
    // is not stored either: the project still reads every default.
    assert_eq!(
        dashboard
            .store
            .project_models("maestro", &StageModels::default())
            .unwrap(),
        StageModels::default()
    );
    assert_eq!(
        dashboard.store.project_efforts("maestro").unwrap(),
        StageEfforts::default()
    );
}

fn agents(clarify: &str, spec: &str, plan: &str, implement: &str, review: &str) -> StageAgents {
    StageAgents {
        clarify: clarify.into(),
        spec: spec.into(),
        plan: plan.into(),
        implement: implement.into(),
        review: review.into(),
    }
}

#[test]
fn a_project_without_agents_reads_claude_for_every_stage() {
    let store = Store::open_in_memory().unwrap();
    let claude = StageAgents::default();
    assert_eq!(claude.clarify, DEFAULT_AGENT.id());
    assert_eq!(claude.review, DEFAULT_AGENT.id());
    assert_eq!(store.project_agents("maestro").unwrap(), claude);
}

#[test]
fn project_agents_are_stored_and_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let saved = agents("claude", "claude", "claude", "claude", "claude");
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.project_agents("maestro").unwrap(),
            StageAgents::default()
        );
        store.save_project_agents("maestro", &saved).unwrap();
    }
    // The saved agents survive a reopen, and another project is untouched.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.project_agents("maestro").unwrap(), saved);
    assert_eq!(
        store.project_agents("other").unwrap(),
        StageAgents::default()
    );
}

#[test]
fn saving_one_projects_agents_leaves_other_projects_alone() {
    let store = Store::open_in_memory().unwrap();
    store
        .save_project_agents("maestro", &StageAgents::default())
        .unwrap();
    store
        .save_project_agents("factory", &StageAgents::default())
        .unwrap();
    // A stray row for one project does not change the other's read.
    assert_eq!(
        store.project_agents("factory").unwrap(),
        StageAgents::default()
    );
    assert_eq!(
        store.project_agents("maestro").unwrap(),
        StageAgents::default()
    );
}

#[test]
fn an_unknown_agent_is_rejected_and_not_stored() {
    let store = Store::open_in_memory().unwrap();
    // An unknown id is refused and writes nothing; codex is an available agent
    // and is accepted (see the codex save test).
    let submitted = agents("claude", "gemini", "claude", "claude", "claude");
    let err = store
        .save_project_agents("maestro", &submitted)
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidAgent(id) if id == "gemini"),
        "{err:?}"
    );
    // Nothing was written, so the project still reads the default agent.
    assert_eq!(
        store.project_agents("maestro").unwrap(),
        StageAgents::default()
    );
}

#[test]
fn codex_is_accepted_for_every_stage() {
    let store = Store::open_in_memory().unwrap();
    let submitted = agents("codex", "codex", "codex", "codex", "codex");
    store.save_project_agents("maestro", &submitted).unwrap();
    assert_eq!(store.project_agents("maestro").unwrap(), submitted);
}

#[test]
fn local_is_accepted_for_a_stage() {
    let store = Store::open_in_memory().unwrap();
    let submitted = agents("local", "claude", "claude", "claude", "claude");
    store.save_project_agents("maestro", &submitted).unwrap();
    assert_eq!(store.project_agents("maestro").unwrap(), submitted);
}

#[test]
fn local_efforts_run_low_through_xhigh_and_reject_max_and_minimal() {
    let store = Store::open_in_memory().unwrap();
    let local = agents("local", "local", "local", "local", "local");
    store
        .save_project_efforts(
            "maestro",
            &efforts("low", "medium", "high", "xhigh", "low"),
            &local,
        )
        .unwrap();
    for level in ["max", "minimal"] {
        let err = store
            .save_project_efforts(
                "maestro",
                &efforts("low", "low", "low", level, "low"),
                &local,
            )
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::InvalidEffort(found) if found == level),
            "{err:?}"
        );
    }
}

#[tokio::test]
async fn saving_a_local_stage_round_trips_and_a_bad_local_stage_stores_nothing() {
    let dashboard = dashboard();
    let form = |model: &str, effort: &str| {
        format!(
            "clarify=&spec=&plan=&implement={model}&review=&\
             clarify_effort=medium&spec_effort=medium&plan_effort=medium&\
             implement_effort={effort}&review_effort=medium&\
             clarify_agent=claude&spec_agent=claude&plan_agent=claude&\
             implement_agent=local&review_agent=claude"
        )
    };

    for (model, effort) in [("gemma", "max"), ("gemma", "minimal"), ("", "high")] {
        let reply = dashboard
            .post("/settings/projects/maestro", &form(model, effort))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{model:?} {effort}");
        assert_eq!(
            dashboard.store.project_agents("maestro").unwrap(),
            StageAgents::default()
        );
        assert_eq!(
            dashboard.store.project_efforts("maestro").unwrap(),
            StageEfforts::default()
        );
        assert_eq!(
            dashboard
                .store
                .project_models("maestro", &StageModels::default())
                .unwrap(),
            StageModels::default()
        );
    }
    let reply = dashboard
        .post("/settings/projects/maestro", &form("", "high"))
        .await;
    assert!(reply.body.contains("The implement stage runs on Local"));

    let reply = dashboard
        .post("/settings/projects/maestro", &form("gemma", "high"))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        dashboard.store.project_agents("maestro").unwrap(),
        agents("claude", "claude", "claude", "local", "claude")
    );
    assert_eq!(
        dashboard.store.project_efforts("maestro").unwrap(),
        efforts("medium", "medium", "medium", "high", "medium")
    );
    assert_eq!(
        dashboard
            .store
            .project_models("maestro", &StageModels::default())
            .unwrap()
            .implement,
        Some("gemma".into())
    );
}

#[test]
fn a_local_session_records_its_cli_once() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    store
        .session_uuid(id, Session::Planning, Agent::Local)
        .unwrap();
    assert_eq!(
        store.session_local_cli(id, Session::Planning).unwrap(),
        None
    );
    store
        .record_session_local_cli(id, Session::Planning, Cli::Codex)
        .unwrap();
    store
        .record_session_local_cli(id, Session::Planning, Cli::Copilot)
        .unwrap();
    assert_eq!(
        store.session_local_cli(id, Session::Planning).unwrap(),
        Some(Cli::Codex)
    );
}

#[test]
fn a_database_from_before_the_local_cli_column_opens_and_keeps_its_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let store = Store::open(&path).unwrap();
        store
            .create_project(
                "maestro",
                &ForgeKind::GitHub,
                "Badger-Embedded/maestro",
                "main",
                None,
                Some("alice"),
            )
            .unwrap();
        store
            .save_project_agents(
                "maestro",
                &agents("codex", "claude", "claude", "claude", "copilot"),
            )
            .unwrap();
        let id = store.create("maestro", "alice", "x").unwrap();
        store
            .session_uuid(id, Session::Planning, Agent::Codex)
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("ALTER TABLE sessions DROP COLUMN local_cli", [])
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.project_agents("maestro").unwrap(),
        agents("codex", "claude", "claude", "claude", "copilot")
    );
    let (_, agent) = store
        .session_uuid(1, Session::Planning, Agent::Claude)
        .unwrap();
    assert_eq!(agent, Agent::Codex);
    assert_eq!(store.session_local_cli(1, Session::Planning).unwrap(), None);
}

#[test]
fn copilot_is_accepted_for_every_stage() {
    let store = Store::open_in_memory().unwrap();
    let submitted = agents("copilot", "copilot", "copilot", "copilot", "copilot");
    store.save_project_agents("maestro", &submitted).unwrap();
    assert_eq!(store.project_agents("maestro").unwrap(), submitted);
}

#[test]
fn a_copilot_session_records_and_keeps_copilot() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    let (uuid, started) = store
        .session_uuid(id, Session::Planning, Agent::Copilot)
        .unwrap();
    assert_eq!(started, Agent::Copilot);
    let (again, resumed) = store
        .session_uuid(id, Session::Planning, Agent::Claude)
        .unwrap();
    assert_eq!(again, uuid);
    assert_eq!(resumed, Agent::Copilot);
}

#[test]
fn copilot_efforts_run_low_through_max_and_reject_other_levels() {
    let store = Store::open_in_memory().unwrap();
    let copilot = agents("copilot", "copilot", "copilot", "copilot", "copilot");
    store
        .save_project_efforts(
            "maestro",
            &efforts("low", "medium", "high", "xhigh", "max"),
            &copilot,
        )
        .unwrap();

    let err = store
        .save_project_efforts(
            "maestro",
            &efforts("minimal", "low", "low", "low", "low"),
            &copilot,
        )
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidEffort(level) if level == "minimal"),
        "{err:?}"
    );
    assert_eq!(
        store.project_efforts("maestro").unwrap(),
        efforts("low", "medium", "high", "xhigh", "max")
    );
}

#[tokio::test]
async fn saving_copilot_for_a_stage_stores_it_and_an_unsupported_effort_stores_nothing() {
    let dashboard = dashboard();
    let form = |effort: &str| {
        format!(
            "clarify=&spec=&plan=&implement=&review=&\
             clarify_effort=medium&spec_effort={effort}&plan_effort=medium&\
             implement_effort=medium&review_effort=medium&\
             clarify_agent=claude&spec_agent=copilot&plan_agent=claude&\
             implement_agent=claude&review_agent=claude"
        )
    };

    let reply = dashboard
        .post("/settings/projects/maestro", &form("minimal"))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        dashboard.store.project_agents("maestro").unwrap(),
        StageAgents::default()
    );
    assert_eq!(
        dashboard.store.project_efforts("maestro").unwrap(),
        StageEfforts::default()
    );

    let reply = dashboard
        .post("/settings/projects/maestro", &form("max"))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        dashboard.store.project_agents("maestro").unwrap(),
        agents("claude", "copilot", "claude", "claude", "claude")
    );
    assert_eq!(
        dashboard.store.project_efforts("maestro").unwrap(),
        efforts("medium", "max", "medium", "medium", "medium")
    );
}

#[test]
fn a_database_from_before_the_agent_table_opens_and_reads_claude() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    // A database as it was before `project_agents` existed: a project but no
    // agent table at all.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                name TEXT PRIMARY KEY,
                repo TEXT NOT NULL UNIQUE,
                base TEXT NOT NULL,
                checks TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO projects (name, repo, base) VALUES ('maestro', 'o/n', 'main');",
        )
        .unwrap();
    }
    // Opening the store creates the table, so the old project reads Claude and a
    // new agent can be saved.
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.project_agents("maestro").unwrap(),
        StageAgents::default()
    );
    let saved = agents("claude", "claude", "claude", "claude", "claude");
    store.save_project_agents("maestro", &saved).unwrap();
    assert_eq!(store.project_agents("maestro").unwrap(), saved);
}

#[test]
fn a_sessions_table_without_the_agent_column_opens_and_resumes_on_claude() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    // A `sessions` table as it was before `agent` joined it, with one row.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                job_id INTEGER NOT NULL,
                session TEXT NOT NULL,
                uuid TEXT NOT NULL,
                PRIMARY KEY (job_id, session)
            );
            INSERT INTO sessions (job_id, session, uuid)
                VALUES (1, 'planning', 'fixed-uuid');",
        )
        .unwrap();
    }
    // Opening the store adds the column with a Claude default, so the old
    // session resumes on Claude even when the stage now names another agent.
    let store = Store::open(&path).unwrap();
    let (uuid, agent) = store
        .session_uuid(1, Session::Planning, Agent::Codex)
        .unwrap();
    assert_eq!(uuid, "fixed-uuid");
    assert_eq!(agent, Agent::Claude);
}

#[test]
fn a_resumed_session_keeps_the_agent_it_started_on() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    // A fresh session records the stage's agent.
    let (first, started) = store
        .session_uuid(id, Session::Planning, Agent::Claude)
        .unwrap();
    assert_eq!(started, Agent::Claude);
    // The stage's agent changes, but the next run of the same session reads the
    // agent that began it, not the stage's current one.
    let (again, resumed) = store
        .session_uuid(id, Session::Planning, Agent::Codex)
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(resumed, Agent::Claude);
    // A different session that starts after the change records the new agent.
    let (_, other) = store.session_uuid(id, Session::Spec, Agent::Codex).unwrap();
    assert_eq!(other, Agent::Codex);
}

#[test]
fn an_agent_session_id_is_stored_against_the_session_and_read_back() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    let (uuid, _) = store
        .session_uuid(id, Session::Planning, Agent::Codex)
        .unwrap();
    // A fresh session has none, the way a run reads before its agent reports one.
    assert_eq!(store.agent_session(id, Session::Planning).unwrap(), None);
    // A session that never ran has no row either.
    assert_eq!(store.agent_session(id, Session::Spec).unwrap(), None);

    store.set_agent_session(&uuid, "thread-42").unwrap();
    assert_eq!(
        store
            .agent_session(id, Session::Planning)
            .unwrap()
            .as_deref(),
        Some("thread-42")
    );
    // The id is keyed by the session's UUID, so it touches no other session.
    store.session_uuid(id, Session::Spec, Agent::Codex).unwrap();
    assert_eq!(store.agent_session(id, Session::Spec).unwrap(), None);

    // A later run refreshes it with the id the agent reports again.
    store.set_agent_session(&uuid, "thread-43").unwrap();
    assert_eq!(
        store
            .agent_session(id, Session::Planning)
            .unwrap()
            .as_deref(),
        Some("thread-43")
    );
}

#[test]
fn a_sessions_table_without_the_agent_session_column_opens_and_reads_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    // A `sessions` table as it was before `agent_session` joined it, with one row.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                job_id INTEGER NOT NULL,
                session TEXT NOT NULL,
                uuid TEXT NOT NULL,
                agent TEXT NOT NULL DEFAULT 'claude',
                PRIMARY KEY (job_id, session)
            );
            INSERT INTO sessions (job_id, session, uuid, agent)
                VALUES (1, 'planning', 'fixed-uuid', 'codex');",
        )
        .unwrap();
    }
    // Opening the store adds the nullable column, so the old session reads no
    // reported id and the next run starts fresh.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.agent_session(1, Session::Planning).unwrap(), None);
    // The column is writable after the migration.
    store.set_agent_session("fixed-uuid", "thread-9").unwrap();
    assert_eq!(
        store
            .agent_session(1, Session::Planning)
            .unwrap()
            .as_deref(),
        Some("thread-9")
    );
}

#[tokio::test]
async fn the_settings_form_has_an_agent_selector_per_stage_with_codex_selectable() {
    let dashboard = dashboard();
    let page = dashboard.get("/settings/projects/maestro").await;
    // Each stage's panel has a radio group named "<stage>_agent".
    for stage in ["clarify", "spec", "plan", "implement", "review"] {
        assert!(
            page.body.contains(&format!("name=\"{stage}_agent\"")),
            "missing the {stage} agent selector",
        );
    }
    // Claude is selectable and preselected in every stage, so the checked
    // Claude radio appears once per stage.
    assert_eq!(
        page.body.matches("value=\"claude\" checked").count(),
        6,
        "Claude should be preselected in all five stages and the Renovate agent card",
    );
    assert!(page.body.contains("value=\"codex\""));
    assert!(!page.body.contains("value=\"codex\" checked"));
    assert!(!page.body.contains("value=\"codex\" disabled"));
    assert_eq!(
        page.body.matches("value=\"copilot\"").count(),
        6,
        "Copilot should be a radio in all five stages and the Renovate agent card",
    );
    assert_eq!(
        page.body.matches("value=\"local\"").count(),
        6,
        "Local should be a radio in all five stages and the Renovate agent card",
    );
    assert!(!page.body.contains("value=\"local\" disabled"));
    assert!(!page.body.contains("value=\"local\" checked"));
    assert!(!page.body.contains("value=\"copilot\" checked"));
    assert!(!page.body.contains("value=\"copilot\" disabled"));
}

#[tokio::test]
async fn saving_the_form_persists_the_agents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with(Arc::clone(&store));

    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&\
             clarify_effort=medium&spec_effort=medium&plan_effort=medium&\
             implement_effort=medium&review_effort=medium&\
             clarify_agent=claude&spec_agent=claude&plan_agent=claude&\
             implement_agent=claude&review_agent=claude",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);

    drop(dashboard);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.project_agents("maestro").unwrap(),
        agents("claude", "claude", "claude", "claude", "claude")
    );
}

#[tokio::test]
async fn an_invalid_agent_submission_is_rejected_and_stores_nothing() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=opus&spec=&plan=&implement=&review=&\
             clarify_effort=medium&spec_effort=high&plan_effort=medium&\
             implement_effort=medium&review_effort=medium&\
             clarify_agent=claude&spec_agent=gemini&plan_agent=claude&\
             implement_agent=claude&review_agent=claude",
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    // Agents are validated before any write, so the model and effort submitted
    // alongside the rejected agent are not stored either.
    assert_eq!(
        dashboard
            .store
            .project_models("maestro", &StageModels::default())
            .unwrap(),
        StageModels::default()
    );
    assert_eq!(
        dashboard.store.project_efforts("maestro").unwrap(),
        StageEfforts::default()
    );
    assert_eq!(
        dashboard.store.project_agents("maestro").unwrap(),
        StageAgents::default()
    );
}

/// The text inside the agent badge of a timeline step, between its span tags.
fn agent_badge(step: &str) -> &str {
    let at = step.find("class=\"step-tag agent\"").unwrap();
    let open = at + step[at..].find('>').unwrap() + 1;
    let close = open + step[open..].find("</span>").unwrap();
    &step[open..close]
}

#[tokio::test]
async fn the_running_step_badge_shows_the_effort_and_stays_compact() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_models("maestro", &models(Some("claude-opus-4-5-20251101"), None))
        .unwrap();
    dashboard
        .store
        .save_project_efforts(
            "maestro",
            &efforts("xhigh", "medium", "medium", "medium", "medium"),
            &StageAgents::default(),
        )
        .unwrap();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let clarifying = timeline_step(&page.body, "clarifying");
    let badge = agent_badge(clarifying);
    // The badge joins the model and the stage's effort with a middot.
    assert_eq!(badge, "claude-opus-4-5-20251101 · xhigh");
    // The effort adds only a short suffix, so the badge stays a compact tag
    // that wraps rather than overflows on a narrow screen.
    assert!(
        badge.chars().count() <= 40,
        "badge too long for a narrow screen: {badge:?}",
    );
}

#[tokio::test]
async fn the_running_step_badge_shows_the_settings_of_a_session_on_another_agent() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard
        .store
        .session_uuid(id, Session::Planning, Agent::Claude)
        .unwrap();
    dashboard
        .store
        .record_session_settings(id, Session::Planning, Some("sonnet"), "high")
        .unwrap();
    let copilot = agents("copilot", "claude", "claude", "claude", "claude");
    dashboard
        .store
        .save_project_efforts(
            "maestro",
            &efforts("low", "medium", "medium", "medium", "medium"),
            &copilot,
        )
        .unwrap();
    dashboard
        .store
        .save_project_agents("maestro", &copilot)
        .unwrap();
    dashboard
        .store
        .save_project_models("maestro", &models(Some("claude-sonnet-5.5"), None))
        .unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let badge = agent_badge(timeline_step(&page.body, "clarifying"));
    assert_eq!(badge, "sonnet · high");

    dashboard
        .slots
        .register_task(id, true, |_| tokio::spawn(std::future::pending()));
    let claude = agents("claude", "claude", "claude", "claude", "claude");
    dashboard
        .store
        .save_project_efforts(
            "maestro",
            &efforts("xhigh", "medium", "medium", "medium", "medium"),
            &claude,
        )
        .unwrap();
    dashboard
        .store
        .save_project_agents("maestro", &claude)
        .unwrap();
    dashboard
        .store
        .save_project_models("maestro", &models(Some("opus"), None))
        .unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let badge = agent_badge(timeline_step(&page.body, "clarifying"));
    assert_eq!(badge, "sonnet · high");
}

#[test]
fn a_sessions_table_without_the_model_and_effort_columns_opens_and_reads_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                job_id INTEGER NOT NULL,
                session TEXT NOT NULL,
                uuid TEXT NOT NULL,
                agent TEXT NOT NULL DEFAULT 'claude',
                agent_session TEXT,
                cost_usd REAL,
                PRIMARY KEY (job_id, session)
            );
            INSERT INTO sessions (job_id, session, uuid)
                VALUES (1, 'planning', 'fixed-uuid');",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let old = store
        .session_settings(1, Session::Planning)
        .unwrap()
        .unwrap();
    assert_eq!(old.agent, Agent::Claude);
    assert_eq!(old.model, None);
    assert_eq!(old.effort, None);
    store
        .record_session_settings(1, Session::Planning, Some("opus"), "low")
        .unwrap();
    let new = store
        .session_settings(1, Session::Planning)
        .unwrap()
        .unwrap();
    assert_eq!(new.model.as_deref(), Some("opus"));
    assert_eq!(new.effort.as_deref(), Some("low"));
}

#[test]
fn project_auto_merge_defaults_off_and_a_saved_row_wins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let store = Store::open(&path).unwrap();
        // No row yet: the project file default decides.
        assert!(!store.project_auto_merge("maestro", false).unwrap());
        assert!(store.project_auto_merge("maestro", true).unwrap());
        store.save_project_auto_merge("maestro", true).unwrap();
    }
    let store = Store::open(&path).unwrap();
    // A saved row replaces the default, whichever way the default points.
    assert!(store.project_auto_merge("maestro", false).unwrap());
    store.save_project_auto_merge("maestro", false).unwrap();
    assert!(!store.project_auto_merge("maestro", true).unwrap());
    // Each project keeps its own value.
    assert!(!store.project_auto_merge("other", false).unwrap());
}

#[test]
fn project_llm_review_defaults_off_and_a_saved_row_wins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let store = Store::open(&path).unwrap();
        // No row yet: the project file default decides.
        assert!(!store.project_llm_review("maestro", false).unwrap());
        assert!(store.project_llm_review("maestro", true).unwrap());
        store.save_project_settings("maestro", false, true).unwrap();
    }
    let store = Store::open(&path).unwrap();
    // A saved row replaces the default, whichever way the default points.
    assert!(store.project_llm_review("maestro", false).unwrap());
    store
        .save_project_settings("maestro", false, false)
        .unwrap();
    assert!(!store.project_llm_review("maestro", true).unwrap());
    // Each project keeps its own value.
    assert!(!store.project_llm_review("other", false).unwrap());
}

#[test]
fn the_two_project_flags_are_stored_and_read_independently() {
    let store = Store::open_in_memory().unwrap();
    store.save_project_settings("maestro", true, false).unwrap();
    assert!(store.project_auto_merge("maestro", false).unwrap());
    assert!(!store.project_llm_review("maestro", true).unwrap());

    store.save_project_settings("maestro", false, true).unwrap();
    assert!(!store.project_auto_merge("maestro", true).unwrap());
    assert!(store.project_llm_review("maestro", false).unwrap());
}

/// A database written before the `llm_review` column existed: `init` adds the
/// column and every project reads off.
#[test]
fn an_old_project_settings_table_gains_the_llm_review_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE project_settings (project TEXT PRIMARY KEY, auto_merge INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO project_settings (project, auto_merge) VALUES ('maestro', 1)",
            [],
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    // The saved auto-merge survives and review reads off for the old row.
    assert!(store.project_auto_merge("maestro", false).unwrap());
    assert!(!store.project_llm_review("maestro", true).unwrap());
}

/// An old settings record with the global `auto_merge` on, as written before
/// the setting became per-project.
const LEGACY_SETTINGS_ON: &str = r#"{"max_workers":2,"worker_image":"i","worker_memory":"3g","worker_cpus":"4","poll_seconds":60,"auto_merge":true,"gates":{"spec":false,"plan":false,"publish":false}}"#;

fn store_legacy_settings(path: &Path, settings: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "INSERT INTO settings (id, settings) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET settings = excluded.settings",
        [settings],
    )
    .unwrap();
}

fn raw_settings(path: &Path) -> String {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT settings FROM settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn the_migration_moves_a_global_auto_merge_into_each_project() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    store_legacy_settings(&path, LEGACY_SETTINGS_ON);

    store
        .migrate_global_auto_merge(&["maestro", "factory"])
        .unwrap();
    // Every listed project keeps the old behaviour.
    assert!(store.project_auto_merge("maestro", false).unwrap());
    assert!(store.project_auto_merge("factory", false).unwrap());
    // The global field is gone from the stored settings.
    assert!(!raw_settings(&path).contains("auto_merge"));
}

#[test]
fn the_migration_runs_once_and_leaves_new_projects_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    store_legacy_settings(&path, LEGACY_SETTINGS_ON);

    store.migrate_global_auto_merge(&["maestro"]).unwrap();
    assert!(store.project_auto_merge("maestro", false).unwrap());
    // The user turns maestro off again.
    store.save_project_auto_merge("maestro", false).unwrap();

    // A later pass, now with a new project, does not re-run: the global field
    // was already dropped, so neither project is turned back on.
    store
        .migrate_global_auto_merge(&["maestro", "newproj"])
        .unwrap();
    assert!(!store.project_auto_merge("maestro", false).unwrap());
    assert!(!store.project_auto_merge("newproj", false).unwrap());
}

#[test]
fn the_migration_with_the_global_off_turns_no_project_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    let off = LEGACY_SETTINGS_ON.replace("\"auto_merge\":true", "\"auto_merge\":false");
    store_legacy_settings(&path, &off);

    store.migrate_global_auto_merge(&["maestro"]).unwrap();
    assert!(!store.project_auto_merge("maestro", false).unwrap());
    assert!(!raw_settings(&path).contains("auto_merge"));
}

#[test]
fn the_migration_without_stored_settings_does_nothing() {
    let store = Store::open_in_memory().unwrap();
    store.migrate_global_auto_merge(&["maestro"]).unwrap();
    assert!(!store.project_auto_merge("maestro", false).unwrap());
    assert_eq!(
        store.settings().unwrap(),
        factory_server::Settings::default()
    );
}

#[tokio::test]
async fn the_settings_page_edits_the_models_of_each_project() {
    let dashboard = dashboard();
    let page = dashboard.settings_pages().await;
    assert!(page.contains("action=\"/settings/projects/maestro?stage=clarify\""));
    for stage in ["clarify", "spec", "plan", "implement", "review"] {
        assert!(page.contains(&format!("name=\"{stage}\"")), "{stage}");
    }

    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=+opus+&spec=&plan=&implement=sonnet&review=",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    assert!(reply.body.contains("value=\"opus\""));
    assert_eq!(
        dashboard
            .store
            .project_models("maestro", &StageModels::default())
            .unwrap(),
        models(Some("opus"), Some("sonnet"))
    );

    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=",
        )
        .await;
    assert_eq!(
        dashboard
            .store
            .project_models("maestro", &models(Some("x"), None))
            .unwrap(),
        StageModels::default()
    );
}

#[tokio::test]
async fn the_settings_page_saves_a_review_model() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=+opus+",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        dashboard
            .store
            .project_models("maestro", &StageModels::default())
            .unwrap()
            .review
            .as_deref(),
        Some("opus")
    );

    // Saving an empty review clears it.
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=",
        )
        .await;
    assert_eq!(
        dashboard
            .store
            .project_models("maestro", &StageModels::default())
            .unwrap()
            .review,
        None
    );
}

#[tokio::test]
async fn models_for_an_unknown_project_are_not_saved() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/nope",
            "clarify=a&spec=&plan=&implement=&review=",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

/// The `<li>` of the timeline step `name`, from its opening tag to the next
/// step's. Panics when the page has no such step.
fn timeline_step<'a>(body: &'a str, name: &str) -> &'a str {
    let timeline = &body[body.find("id=\"job-timeline\"").unwrap()..];
    let label = format!("<span class=\"step-name\">{name}</span>");
    let at = timeline.find(&label).unwrap();
    let start = timeline[..at].rfind("<li").unwrap();
    let end = timeline[at..]
        .find("<li class=")
        .map_or(timeline.len(), |end| at + end);
    &timeline[start..end]
}

#[tokio::test]
async fn the_timeline_marks_the_running_step_with_its_model_and_task() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_models("maestro", &models(Some("opus"), None))
        .unwrap();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let clarifying = timeline_step(&page.body, "clarifying");
    assert!(clarifying.contains("class=\"current\""));
    assert!(clarifying.contains("aria-current=\"step\""));
    // The badge shows the model and, after it, the effort for the stage.
    assert!(clarifying.contains(">opus · medium</span>"));
    assert!(clarifying.contains("reading the repository"));
    assert!(clarifying.contains("since "));
    assert!(timeline_step(&page.body, "requested").contains("class=\"done\""));
    assert!(timeline_step(&page.body, "spec").contains("class=\"pending\""));
    assert_eq!(page.body.matches("step-tag gate").count(), 2);
    assert!(page.body.contains("the planned pull requests"));
}

#[tokio::test]
async fn the_running_step_shows_the_full_model_name() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_models("maestro", &models(None, Some("claude-opus-4-5-20251101")))
        .unwrap();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![planned(
            "maestrod: add a reboot endpoint",
            None,
        )])),
    );
    dashboard.post(&format!("/jobs/{id}/approve"), "").await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let implementing = timeline_step(&page.body, "implementing");
    assert!(implementing.contains(">claude-opus-4-5-20251101 · medium</span>"));
    assert!(implementing.contains("implementing the pull request"));
}

#[tokio::test]
async fn a_job_without_gates_shows_no_gate_label() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_gates("maestro", no_gates())
        .unwrap();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("id=\"job-timeline\""));
    assert!(!page.body.contains("step-tag gate"));
}

#[tokio::test]
async fn an_approved_gate_shows_when_it_was_approved() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard
        .post(&format!("/jobs/{id}/changes"), "feedback=shorter")
        .await;
    dashboard.apply(id, Event::Agent(AgentResult::Spec("Goal: short.".into())));
    dashboard.post(&format!("/jobs/{id}/approve"), "").await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let spec = timeline_step(&page.body, "spec");
    assert!(spec.contains("class=\"done\""));
    assert!(spec.contains("✓ approved "));
    assert!(!spec.contains("step-tag gate"));
    assert!(spec.contains("You asked for changes"));
    let plan = timeline_step(&page.body, "plan");
    assert!(plan.contains("class=\"current\""));
    assert!(plan.contains("step-tag gate"));
}

#[tokio::test]
async fn each_planned_pull_request_has_its_own_group() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![
            planned("store: add the table", None),
            planned("pages: show it", None),
        ])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.store.add_cost(id, 1.25, false).unwrap();
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 81 });
    dashboard.apply(id, Event::CiFailed { log: "red".into() });
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 81 });
    dashboard.apply(id, Event::Merged);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let timeline = &page.body[page.body.find("id=\"job-timeline\"").unwrap()..];
    let first = timeline.find("PR 1 of 2 · #81").unwrap();
    let second = timeline.find("PR 2 of 2").unwrap();
    assert!(first < second);
    // The merged pull request's group is collapsed, the running one open.
    let groups: Vec<&str> = timeline
        .split("<details class=\"pr-group\"")
        .skip(1)
        .collect();
    assert_eq!(groups.len(), 2);
    assert!(!groups[0].starts_with(" open"));
    assert!(groups[0].contains("store: add the table"));
    assert!(groups[0].contains("CI failed"));
    assert!(groups[0].contains("$1.25"));
    assert!(groups[1].starts_with(" open"));
    assert!(groups[1].contains("class=\"current\""));
}

#[tokio::test]
async fn a_skipped_pull_request_says_so_on_the_job_page() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![
            planned("worker: install the tool", None),
            planned("core: use it", None),
        ])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::NothingToPublish);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let timeline = &page.body[page.body.find("id=\"job-timeline\"").unwrap()..];
    assert!(timeline.contains("PR 1 of 2 · skipped"), "{timeline}");
    assert!(
        timeline.contains("No commits to publish, so the factory skipped it"),
        "{timeline}"
    );
    assert!(!timeline.contains("PR 2 of 2 · skipped"), "{timeline}");
    assert!(
        page.body.contains("· skipped: nothing to publish"),
        "{}",
        page.body
    );
}

#[tokio::test]
async fn a_failed_job_marks_the_step_it_failed_in() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let step = timeline_step(&page.body, "ci-and-review");
    assert!(step.contains("class=\"failed\""));
    assert!(step.contains("5 failed check or CI rounds on one pull request"));
    assert_eq!(
        step.matches("CI failed").count(),
        MAX_FAILED_ROUNDS as usize
    );
    assert!(timeline_step(&page.body, "merged").contains("class=\"pending\""));
}

#[tokio::test]
async fn a_job_from_before_the_history_shows_its_steps_without_times() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with(store);
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM job_events", [])
        .unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    let clarifying = timeline_step(&page.body, "clarifying");
    assert!(clarifying.contains("class=\"done\""));
    assert!(clarifying.contains("<span class=\"when\"></span>"));
    assert!(timeline_step(&page.body, "spec").contains("class=\"current\""));
}

#[tokio::test]
async fn the_job_fragment_swaps_the_timeline() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let reply = dashboard.get(&format!("/jobs/{id}/fragment")).await;
    assert!(
        reply
            .body
            .contains("<div id=\"job-timeline\" hx-swap-oob=\"true\">")
    );
}

/// A job that failed because it used up its failed rounds while watching the
/// one pull request #7.
async fn failed_out_of_rounds(dashboard: &Dashboard) -> i64 {
    let id = dashboard.new_job().await;
    to_spec_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![planned("only", None)])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 7 });
    for _ in 1..MAX_FAILED_ROUNDS {
        dashboard.apply(id, Event::CiFailed { log: "fail".into() });
        dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
        dashboard.apply(id, Event::ChecksPassed);
        dashboard.apply(id, Event::Published { number: 7 });
    }
    dashboard.apply(id, Event::CiFailed { log: "last".into() });
    assert!(matches!(
        dashboard.store.get(id).unwrap().job.stage(),
        Stage::Failed { .. }
    ));
    id
}

#[tokio::test]
async fn the_retry_endpoint_resumes_a_failed_job_and_logs_it() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let reply = dashboard.post(&format!("/jobs/{id}/retry"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location, Some(format!("/jobs/{id}")));
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::CiAndReview { pr: 0 }
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Retried the job from the dashboard."));
}

#[tokio::test]
async fn retrying_from_the_board_returns_to_the_selected_project() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let reply = dashboard
        .post(&format!("/jobs/{id}/retry"), "project=maestro")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/?project=maestro"));
}

#[tokio::test]
async fn the_failure_card_shows_a_retry_button_for_a_retryable_job() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains(&format!("/jobs/{id}/retry")));
    assert!(page.body.contains("Retry"));
}

#[tokio::test]
async fn a_retry_reruns_a_failed_agent_run() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![planned("only", None)])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The job failed"));
    assert!(page.body.contains(&format!("/jobs/{id}/retry")));

    let reply = dashboard.post(&format!("/jobs/{id}/retry"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Implementing { pr: 0 }
    );
}

const CHECKS_HINT: &str = "Revise the project's local checks command, then retry";

#[tokio::test]
async fn a_job_whose_checks_could_not_run_links_to_the_project_settings() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![planned("only", None)])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    for _ in 0..5 {
        dashboard.apply(
            id,
            Event::ChecksError {
                error: "bash: just: command not found".into(),
            },
        );
    }

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The job failed"));
    assert!(page.body.contains(CHECKS_HINT));
    assert!(page.body.contains("href=\"/settings/projects/maestro\""));
    assert!(page.body.contains(&format!("/jobs/{id}/retry")));
}

#[tokio::test]
async fn a_job_that_failed_another_way_shows_no_checks_hint() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![planned("only", None)])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The job failed"));
    assert!(!page.body.contains(CHECKS_HINT));
    assert!(!page.body.contains("/settings/projects/"));
}

#[tokio::test]
async fn a_closed_unmerged_job_offers_no_retry_button_and_rejects_retry() {
    let dashboard = dashboard();
    let id = failed_without_retry(&dashboard).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The job failed"));
    assert!(!page.body.contains(&format!("/jobs/{id}/retry")));

    let reply = dashboard.post(&format!("/jobs/{id}/retry"), "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
}

/// A failed job that is not retryable: its pull request was closed without
/// merging.
async fn failed_without_retry(dashboard: &Dashboard) -> i64 {
    let id = dashboard.new_job().await;
    to_spec_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![planned("only", None)])),
    );
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 7 });
    dashboard.apply(id, Event::ClosedUnmerged);
    id
}

#[tokio::test]
async fn a_failed_job_sits_in_its_own_expanded_lane_above_done() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;
    let cancelled = dashboard.new_job().await;
    dashboard.apply(cancelled, Event::Cancel);

    let body = dashboard.get("/").await.body;
    assert!(body.contains("<section class=\"inbox inbox-failed\">"));
    assert!(body.contains("Failed <span class=\"count\">1</span>"));
    assert_eq!(lanes_of(&body, id), ["inbox-failed"]);
    // The Failed lane comes before the collapsed Done lane.
    assert!(
        body.find("class=\"inbox inbox-failed\"").unwrap()
            < body.find("class=\"inbox inbox-done\"").unwrap()
    );
}

#[tokio::test]
async fn the_failed_lane_is_hidden_with_no_empty_message_when_nothing_failed() {
    let dashboard = dashboard();
    dashboard.new_job().await;
    let body = dashboard.get("/").await.body;
    assert!(!body.contains("inbox-failed"));
    assert!(!body.contains("No failed jobs."));
    assert!(!body.contains("Failed <span"));
}

#[tokio::test]
async fn done_counts_merged_and_cancelled_but_not_failed_jobs() {
    let dashboard = dashboard();
    let merged = dashboard.new_job().await;
    merge(&dashboard, merged);
    let cancelled = dashboard.new_job().await;
    dashboard.apply(cancelled, Event::Cancel);
    failed_out_of_rounds(&dashboard).await;

    let body = dashboard.get("/").await.body;
    assert!(body.contains("Done <span class=\"count\">2</span>"));
    assert!(body.contains("Failed <span class=\"count\">1</span>"));
    assert_eq!(lanes_of(&body, merged), ["inbox-done"]);
    assert_eq!(lanes_of(&body, cancelled), ["inbox-done"]);
}

#[tokio::test]
async fn a_failed_card_shows_a_retry_button_that_returns_to_the_board() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "inbox-failed");
    assert!(lane.contains(&format!("action=\"/jobs/{id}/retry\"")));
    assert!(lane.contains("name=\"project\" value=\"maestro\""));
    assert!(lane.contains(">Retry</button>"));

    let reply = dashboard
        .post(&format!("/jobs/{id}/retry"), "project=maestro")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/?project=maestro"));
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::CiAndReview { pr: 0 }
    );
}

#[tokio::test]
async fn a_failed_card_without_a_retry_target_shows_no_button() {
    let dashboard = dashboard();
    let id = failed_without_retry(&dashboard).await;

    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "inbox-failed");
    assert!(lane.contains(&format!("href=\"/jobs/{id}\"")));
    assert!(!lane.contains("/retry"));
    assert!(!lane.contains("Retry"));
}

#[tokio::test]
async fn a_failed_row_shows_a_dismiss_button_next_to_retry() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "inbox-failed");
    assert!(lane.contains(&format!("action=\"/jobs/{id}/retry\"")));
    assert!(lane.contains(">Retry</button>"));
    assert!(lane.contains(&format!("action=\"/jobs/{id}/dismiss\"")));
    assert!(lane.contains(">Dismiss</button>"));
    assert!(lane.contains("name=\"project\" value=\"maestro\""));
}

#[tokio::test]
async fn a_failed_row_that_cannot_retry_shows_only_dismiss() {
    let dashboard = dashboard();
    let id = failed_without_retry(&dashboard).await;

    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "inbox-failed");
    assert!(!lane.contains("/retry"));
    assert!(!lane.contains("Retry"));
    assert!(lane.contains(&format!("action=\"/jobs/{id}/dismiss\"")));
    assert!(lane.contains(">Dismiss</button>"));
}

#[tokio::test]
async fn a_dismissed_job_moves_to_done_with_a_dismissed_badge_and_no_button() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;
    dashboard.apply(id, Event::Dismiss);

    let body = dashboard.get("/").await.body;
    assert_eq!(lanes_of(&body, id), ["inbox-done"]);
    let done = lane_html(&body, "inbox-done");
    assert!(done.contains(">dismissed</span>"));
    assert!(!done.contains("5 failed check or CI rounds on one pull request"));
    assert!(!done.contains("/dismiss"));
    assert!(!done.contains("/retry"));
    assert!(!body.contains("inbox-failed"));
}

#[tokio::test]
async fn dismissing_from_the_board_returns_to_the_selected_project() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let reply = dashboard
        .post(&format!("/jobs/{id}/dismiss"), "project=maestro")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/?project=maestro"));
    assert!(dashboard.store.get(id).unwrap().job.is_dismissed());

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Dismissed the job from the dashboard."));
}

#[tokio::test]
async fn dismissing_from_the_job_page_redirects_to_the_job() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let reply = dashboard.post(&format!("/jobs/{id}/dismiss"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location, Some(format!("/jobs/{id}")));
}

#[tokio::test]
async fn the_failure_card_dismiss_button_toggles_with_the_state() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The job failed"));
    assert!(page.body.contains(&format!("action=\"/jobs/{id}/retry\"")));
    assert!(
        page.body
            .contains(&format!("action=\"/jobs/{id}/dismiss\""))
    );

    dashboard.apply(id, Event::Dismiss);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The job was dismissed"));
    assert!(page.body.contains(">dismissed</span>"));
    assert!(
        page.body
            .contains("5 failed check or CI rounds on one pull request")
    );
    assert!(page.body.contains(&format!("action=\"/jobs/{id}/retry\"")));
    assert!(
        !page
            .body
            .contains(&format!("action=\"/jobs/{id}/dismiss\""))
    );
}

#[tokio::test]
async fn a_dismissed_job_that_cannot_retry_keeps_the_reason_but_no_button() {
    let dashboard = dashboard();
    let id = failed_without_retry(&dashboard).await;
    dashboard.apply(id, Event::Dismiss);

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("The job was dismissed"));
    assert!(
        page.body
            .contains("the pull request was closed without merging")
    );
    assert!(!page.body.contains(&format!("action=\"/jobs/{id}/retry\"")));
    assert!(
        !page
            .body
            .contains(&format!("action=\"/jobs/{id}/dismiss\""))
    );
}

#[tokio::test]
async fn dismissing_a_non_failed_or_dismissed_job_is_rejected() {
    let dashboard = dashboard();

    let live = dashboard.new_job().await;
    to_spec_approval(&dashboard, live);
    let reply = dashboard.post(&format!("/jobs/{live}/dismiss"), "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);

    let merged = dashboard.new_job().await;
    merge(&dashboard, merged);
    let reply = dashboard.post(&format!("/jobs/{merged}/dismiss"), "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);

    let failed = failed_out_of_rounds(&dashboard).await;
    dashboard.apply(failed, Event::Dismiss);
    let reply = dashboard.post(&format!("/jobs/{failed}/dismiss"), "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(dashboard.store.get(failed).unwrap().job.is_dismissed());
}

#[tokio::test]
async fn retrying_a_dismissed_job_clears_the_dismissal_and_leaves_done() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;
    dashboard.apply(id, Event::Dismiss);

    let reply = dashboard.post(&format!("/jobs/{id}/retry"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let job = dashboard.store.get(id).unwrap().job;
    assert!(!job.is_dismissed());
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });

    let body = dashboard.get("/").await.body;
    assert_ne!(lanes_of(&body, id), ["inbox-done"]);
}

#[tokio::test]
async fn a_failed_job_still_counts_as_finished_for_the_30_day_expiry() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;
    // The Failed lane only changes where the board shows the job. The expiry
    // loop deletes it once it is 30 days old, because it is still finished.
    assert!(dashboard.store.get(id).unwrap().job.is_finished());
    assert_lane(&dashboard, id, "inbox-failed").await;
}

/// A `rate_limit_info` object with all three windows. The reset times are in
/// 2100, so the tile shows them as upcoming.
const USAGE_WITH_FABLE: &str = r#"{ "unifiedWindows": {
    "five_hour": { "utilization": 0.42, "resetsAt": 4102444800 },
    "seven_day": { "utilization": 0.73, "resetsAt": 4102531200 },
    "seven_day_overage_included": { "utilization": 0.1, "resetsAt": 4102531200 }
} }"#;

/// The same object with no Fable window.
const USAGE_NO_FABLE: &str = r#"{ "unifiedWindows": {
    "five_hour": { "utilization": 0.42, "resetsAt": 4102444800 },
    "seven_day": { "utilization": 0.73, "resetsAt": 4102531200 }
} }"#;

fn usage(body: &str) -> PlanUsage {
    PlanUsage::parse(body).unwrap()
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// 2026-11-21 00:00 UTC, the report time the tile tests record.
const REPORTED_AT: i64 = 1_795_219_200;

/// The plan usage panel on the page, or `None` when the tile is hidden.
fn usage_tile(body: &str) -> Option<&str> {
    let marker = "<section class=\"plan-usage\">";
    let start = body.find(marker)?;
    let rest = &body[start..];
    Some(&rest[..rest.find("</section>").unwrap() + "</section>".len()])
}

#[tokio::test]
async fn the_board_shows_the_plan_usage_tile_with_fable() {
    let dashboard = dashboard();
    dashboard.usage.record(usage(USAGE_WITH_FABLE), REPORTED_AT);

    let body = dashboard.get("/").await.body;
    let tile = usage_tile(&body).expect("the tile shows after a run reports usage");
    assert!(tile.contains("Claude plan usage"));
    assert!(tile.contains("5-hour"));
    assert!(tile.contains("Weekly"));
    assert!(tile.contains("Fable"));
    assert!(tile.contains("42% used"));
    assert!(tile.contains("73% used"));
    assert!(tile.contains("10% used"));
    // The report and reset times are wrapped so the browser shows them in its
    // own zone, with the stored UTC time as the fallback for a visitor with
    // JavaScript off.
    assert!(tile.contains(
        "as of <time datetime=\"2026-11-21T00:00:00Z\" data-format=\"datetime\">2026-11-21 00:00 UTC</time>"
    ));
    assert!(tile.contains(
        "resets <time datetime=\"2100-01-01T00:00:00Z\" data-format=\"datetime\">2100-01-01 00:00 UTC</time>"
    ));
    assert!(tile.contains(
        "<time datetime=\"2100-01-02T00:00:00Z\" data-format=\"datetime\">2100-01-02 00:00 UTC</time>"
    ));
}

#[tokio::test]
async fn the_tile_shows_the_two_windows_when_no_fable_window_is_reported() {
    let dashboard = dashboard();
    dashboard.usage.record(usage(USAGE_NO_FABLE), REPORTED_AT);

    let body = dashboard.get("/").await.body;
    let tile = usage_tile(&body).expect("the tile shows without a Fable window");
    assert!(tile.contains("5-hour"));
    assert!(tile.contains("Weekly"));
    assert!(!tile.contains("Fable"));
}

#[tokio::test]
async fn an_empty_cache_hides_the_tile_and_renders_the_rest() {
    let dashboard = dashboard();
    dashboard.new_job().await;

    let body = dashboard.get("/").await.body;
    assert!(usage_tile(&body).is_none());
    assert!(!body.contains("Claude plan usage"));
    // The rest of the board renders.
    assert!(body.contains("Add a reboot button"));
    assert!(body.contains("id=\"board\""));
}

#[tokio::test]
async fn an_old_report_stays_on_the_tile_with_its_time() {
    let dashboard = dashboard();
    dashboard
        .usage
        .record(usage(USAGE_WITH_FABLE), 1_000_000_000);

    let body = dashboard.get("/").await.body;
    let tile = usage_tile(&body).expect("an old report still shows");
    assert!(tile.contains("as of <time datetime=\"2001-09-09T01:46:00Z\""));
    assert!(tile.contains("42% used"));
}

#[tokio::test]
async fn a_window_past_its_reset_time_reads_as_reset() {
    let dashboard = dashboard();
    let body = r#"{ "unifiedWindows": {
        "five_hour": { "utilization": 0.9, "resetsAt": 1700000000 },
        "seven_day": { "utilization": 0.73, "resetsAt": 4102531200 }
    } }"#;
    dashboard.usage.record(usage(body), REPORTED_AT);

    let page = dashboard.get("/").await.body;
    let tile = usage_tile(&page).expect("the tile shows");
    assert!(!tile.contains("90% used"));
    assert!(tile.contains("no report since the reset"));
    assert!(tile.contains(
        "reset <time datetime=\"2023-11-14T22:13:00Z\" data-format=\"datetime\">2023-11-14 22:13 UTC</time>"
    ));
    assert!(tile.contains("73% used"));
}

#[tokio::test]
async fn a_later_report_replaces_only_the_windows_it_carries() {
    let dashboard = dashboard();
    dashboard.usage.record(usage(USAGE_WITH_FABLE), REPORTED_AT);
    let five_hour =
        r#"{ "rateLimitType": "five_hour", "utilization": 0.5, "resetsAt": 4102444800 }"#;
    dashboard.usage.record(usage(five_hour), REPORTED_AT + 60);

    let body = dashboard.get("/").await.body;
    let tile = usage_tile(&body).expect("the tile shows");
    assert!(tile.contains("50% used"));
    assert!(tile.contains("73% used"));
    assert!(tile.contains("as of <time datetime=\"2026-11-21T00:01:00Z\""));
}

#[tokio::test]
async fn the_tile_is_the_same_whatever_project_is_selected() {
    let dashboard = two_projects();
    dashboard.usage.record(usage(USAGE_WITH_FABLE), REPORTED_AT);

    let maestro = dashboard.get("/?project=maestro").await.body;
    let tool = dashboard.get("/?project=tool").await.body;
    let maestro_tile = usage_tile(&maestro).expect("the tile shows for maestro");
    let tool_tile = usage_tile(&tool).expect("the tile shows for tool");
    assert_eq!(maestro_tile, tool_tile);
    assert!(maestro_tile.contains("Fable"));
}

/// The plan usage wrapper on the board, from its id back to the `div` that
/// opens it and forward to the `div` that closes it.
fn usage_wrapper(body: &str) -> &str {
    let id = body.find("id=\"usage\"").unwrap();
    let open = body[..id].rfind("<div").unwrap();
    let rest = &body[open..];
    &rest[..rest.find("</div>").unwrap() + "</div>".len()]
}

#[tokio::test]
async fn each_board_region_polls_its_fragment_every_five_seconds() {
    let dashboard = dashboard();
    let body = dashboard.get("/").await.body;
    // The plan usage wrapper keeps a fixed URL and sends its version in
    // `hx-vals`; the other four carry the version in the poll URL as `&v=`.
    assert!(body.contains("hx-get=\"/fragments/plan-usage\""));
    for url in [
        "/fragments/stats?project=maestro",
        "/fragments/board?project=maestro",
        "/fragments/workers?project=maestro",
    ] {
        assert!(body.contains(&format!("hx-get=\"{url}&amp;v=")), "{url}");
    }
    // One trigger per region: the worker tiles, the usage meters, the job
    // rows, the totals line and the project picker's list.
    assert_eq!(body.matches("hx-trigger=\"every 5s\"").count(), 5);
    assert!(!body.contains("/fragments/lanes"));
}

/// The worker tiles on a page, from the section that holds them to its end.
fn worker_tiles(body: &str) -> &str {
    let start = body.find("<section class=\"workers\"").unwrap();
    let rest = &body[start..];
    &rest[..rest.find("</section>").unwrap()]
}

#[tokio::test]
async fn the_board_shows_one_idle_tile_per_worker_slot() {
    let body = dashboard().get("/").await.body;
    let tiles = worker_tiles(&body);
    assert!(tiles.contains("hx-get=\"/fragments/workers?project=maestro&amp;v="));
    assert_eq!(tiles.matches("<li class=\"worker idle\">").count(), 2);
    assert!(tiles.contains("Worker 1"));
    assert!(tiles.contains("Worker 2"));
    assert!(!tiles.contains("Worker 3"));
}

#[tokio::test]
async fn the_tile_count_follows_the_worker_limit_setting() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_settings(&Settings {
            max_workers: 3,
            ..Settings::default()
        })
        .unwrap();
    let body = dashboard.get("/").await.body;
    assert_eq!(
        worker_tiles(&body).matches("class=\"worker idle\"").count(),
        3
    );
}

#[tokio::test]
async fn a_busy_tile_names_the_job_its_stage_and_how_long_it_has_run() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard.slots.take(id, now() - 34 * 60 - 5);

    let body = dashboard.get("/").await.body;
    let tiles = worker_tiles(&body);
    assert_eq!(tiles.matches("class=\"worker busy\"").count(), 1);
    assert_eq!(tiles.matches("class=\"worker idle\"").count(), 1);
    assert!(tiles.contains(&format!("href=\"/jobs/{id}\"")));
    assert!(tiles.contains(&format!("#{id} · clarifying")));
    assert!(tiles.contains("34 min"));
    // Worker 1 is busy and comes before the idle Worker 2.
    assert!(tiles.find("worker busy").unwrap() < tiles.find("worker idle").unwrap());
}

#[tokio::test]
async fn a_long_run_shows_hours_and_a_new_one_under_a_minute() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    dashboard.slots.take(first, now() - (2 * 60 + 5) * 60);
    dashboard.slots.take(second, now());
    let tiles = dashboard
        .get("/fragments/workers?project=maestro")
        .await
        .body;
    assert!(tiles.contains("2 h 5 min"));
    assert!(tiles.contains("under a minute"));
}

#[tokio::test]
async fn a_tile_for_another_projects_job_names_that_project() {
    let dashboard = two_projects();
    dashboard.post_job("tool", "Tool change", &[]).await;
    dashboard.slots.take(1, now());

    let tool = dashboard.get("/?project=tool").await.body;
    assert!(worker_tiles(&tool).contains("#1 · requested"));
    let maestro = dashboard.get("/?project=maestro").await.body;
    assert!(worker_tiles(&maestro).contains("#1 tool · requested"));
}

const MAESTRO_ONLY: &str = "carol@example.com";

#[tokio::test]
async fn a_busy_tile_hides_a_job_in_a_project_the_viewer_cannot_see() {
    let dashboard = two_projects();
    dashboard
        .store
        .add_collaborator("maestro", MAESTRO_ONLY, "test")
        .unwrap();
    let reply = dashboard.post_job("tool", "Secret tool change", &[]).await;
    let id = job_id(&reply);
    dashboard.apply(id, Event::Start);
    dashboard.slots.take(id, now() - 5 * 60);

    for uri in ["/?project=maestro", "/fragments/workers?project=maestro"] {
        let body = dashboard.get_as(MAESTRO_ONLY, uri).await.body;
        let tiles = worker_tiles(&body);
        assert_eq!(tiles.matches("class=\"worker busy\"").count(), 1, "{uri}");
        assert!(tiles.contains("5 min"), "{uri}");
        assert!(!tiles.contains(&format!("/jobs/{id}")), "{uri}");
        assert!(!tiles.contains(&format!("#{id}")), "{uri}");
        assert!(!tiles.contains("tool"), "{uri}");
        assert!(!tiles.contains("Secret tool change"), "{uri}");
        assert!(!tiles.contains("clarifying"), "{uri}");
    }
}

#[tokio::test]
async fn the_workers_fragment_is_only_the_tiles_and_keeps_polling() {
    let dashboard = dashboard();
    let reply = dashboard.get("/fragments/workers?project=maestro").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.starts_with("<section class=\"workers\""));
    assert!(reply.body.contains("hx-trigger=\"every 5s\""));
    assert!(!reply.body.contains("action=\"/jobs\""));
}

#[tokio::test]
async fn the_lanes_fragment_is_gone() {
    let reply = dashboard().get("/fragments/lanes?project=maestro").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_new_request_form_is_outside_every_swapped_region() {
    let dashboard = dashboard();
    let body = dashboard.get("/").await.body;
    let form = body.find("action=\"/jobs\"").unwrap();
    // The job rows and the totals line come after the form, so a poll that
    // swaps one never touches the form.
    for id in ["id=\"stats\"", "id=\"board\""] {
        assert!(body.find(id).unwrap() > form, "{id} is not after the form");
    }
    // The worker tiles sit above the form and do not hold it.
    assert!(!worker_tiles(&body).contains("<form"));
    // The plan usage wrapper above the form holds the tile, not the form.
    assert!(!usage_wrapper(&body).contains("<form"));
}

#[tokio::test]
async fn the_new_request_form_is_a_one_line_bar_with_the_image_picker() {
    let body = dashboard().get("/").await.body;
    assert!(body.contains("<form class=\"composer\""));
    assert!(body.contains("rows=\"1\""));
    assert!(body.contains("placeholder=\"Describe a change for maestro…\""));
    assert!(body.contains("New request for maestro"));
    assert!(body.contains("id=\"images\""));
    assert!(body.contains("id=\"pasted-images\""));
}

#[tokio::test]
async fn a_board_fragment_is_a_plain_get_of_only_that_region() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let reply = dashboard.get("/fragments/board?project=maestro").await;
    assert_eq!(reply.status, StatusCode::OK);
    // Only the board region: no page chrome and no new-request form.
    assert!(reply.body.trim_start().starts_with("<div class=\"board\""));
    assert!(!reply.body.contains("<html"));
    assert!(!reply.body.contains("<head"));
    assert!(!reply.body.contains("action=\"/jobs\""));
    assert!(reply.body.contains(&format!("href=\"/jobs/{id}\"")));
    // The returned region carries its own polling attributes, so the swap
    // keeps it polling.
    assert!(
        reply
            .body
            .contains("hx-get=\"/fragments/board?project=maestro&amp;v=")
    );
}

#[tokio::test]
async fn a_new_job_shows_up_in_the_board_fragment() {
    let dashboard = dashboard();
    let before = dashboard.get("/fragments/board?project=maestro").await.body;
    assert!(!before.contains("Add a reboot button"));

    let id = dashboard.new_job().await;
    let after = dashboard.get("/fragments/board?project=maestro").await.body;
    assert!(after.contains(&format!("href=\"/jobs/{id}\"")));
    assert!(after.contains("Add a reboot button"));
}

#[tokio::test]
async fn a_lane_change_shows_up_in_the_board_fragment() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let board = dashboard.get("/fragments/board?project=maestro").await.body;
    assert_eq!(lanes_of(&board, id), ["inbox-running"]);

    to_spec_approval(&dashboard, id);
    // The spec is ready, so the same job moves to Needs you.
    let board = dashboard.get("/fragments/board?project=maestro").await.body;
    assert_eq!(lanes_of(&board, id), ["inbox-you"]);
}

#[tokio::test]
async fn the_plan_usage_fragment_appears_once_a_run_reports() {
    let dashboard = dashboard();
    // An empty cache: the fragment is empty, so a swap leaves no tile.
    let empty = dashboard.get("/fragments/plan-usage").await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(!empty.body.contains("Claude plan usage"));

    dashboard.usage.record(usage(USAGE_WITH_FABLE), REPORTED_AT);
    let reported = dashboard.get("/fragments/plan-usage").await.body;
    assert!(reported.contains("<section class=\"plan-usage\">"));
    assert!(reported.contains("Claude plan usage"));
    assert!(reported.contains("Fable"));
}

#[tokio::test]
async fn the_plan_usage_fragment_404s_for_a_project_the_login_cannot_see() {
    let dashboard = two_projects();
    dashboard.usage.record(usage(USAGE_WITH_FABLE), REPORTED_AT);

    assert_eq!(
        dashboard
            .get_as(STRANGER, "/fragments/plan-usage?project=maestro")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let none = dashboard.get_as(STRANGER, "/fragments/plan-usage").await;
    assert_eq!(none.status, StatusCode::OK);
    assert!(!none.body.contains("plan-usage"));

    dashboard
        .store
        .add_collaborator("tool", STRANGER, "test")
        .unwrap();
    assert_eq!(
        dashboard
            .get_as(STRANGER, "/fragments/plan-usage?project=maestro")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// The version a region's markup carries in its poll URL. The `&` renders as
/// `&amp;` in the attribute, so the marker matches the escaped form.
fn url_version(body: &str) -> String {
    let marker = "&amp;v=";
    let start = body.find(marker).expect("a region version") + marker.len();
    body[start..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect()
}

/// The version the plan usage tile carries in its hidden marker, which the
/// wrapper's `hx-vals` sends on the next poll.
fn data_version(body: &str) -> String {
    let marker = "data-version=\"";
    let start = body.find(marker).expect("a usage version") + marker.len();
    body[start..].chars().take_while(|c| *c != '"').collect()
}

#[tokio::test]
async fn a_board_poll_with_the_current_version_is_a_204() {
    let dashboard = dashboard();
    dashboard.new_job().await;
    let region = dashboard.get("/fragments/board?project=maestro").await;
    assert_eq!(region.status, StatusCode::OK);
    let version = url_version(&region.body);

    // The version the page already shows: nothing to redraw, so htmx is told
    // not to swap with an empty 204.
    let same = dashboard
        .get(&format!("/fragments/board?project=maestro&v={version}"))
        .await;
    assert_eq!(same.status, StatusCode::NO_CONTENT);
    assert_eq!(same.body, "");

    // A stale version, and a missing one, get the region markup to swap in.
    let stale = dashboard
        .get("/fragments/board?project=maestro&v=deadbeefdeadbeef")
        .await;
    assert_eq!(stale.status, StatusCode::OK);
    assert!(stale.body.contains("class=\"board\""));
    let missing = dashboard.get("/fragments/board?project=maestro").await;
    assert_eq!(missing.status, StatusCode::OK);
    assert!(missing.body.contains("class=\"board\""));
}

#[tokio::test]
async fn the_board_version_changes_when_a_job_changes_stage() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let before = url_version(&dashboard.get("/fragments/board?project=maestro").await.body);

    to_spec_approval(&dashboard, id);
    let after = url_version(&dashboard.get("/fragments/board?project=maestro").await.body);
    assert_ne!(
        before, after,
        "the rendered markup changed, so its version must"
    );

    // The version from before the change is now stale, so the poll that sent
    // it gets the new markup rather than a 204.
    let stale = dashboard
        .get(&format!("/fragments/board?project=maestro&v={before}"))
        .await;
    assert_eq!(stale.status, StatusCode::OK);
    let current = dashboard
        .get(&format!("/fragments/board?project=maestro&v={after}"))
        .await;
    assert_eq!(current.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn the_worker_and_stats_polls_answer_204_when_unchanged() {
    let dashboard = dashboard();
    for region in ["workers", "stats"] {
        let url = format!("/fragments/{region}?project=maestro");
        let version = url_version(&dashboard.get(&url).await.body);
        let same = dashboard.get(&format!("{url}&v={version}")).await;
        assert_eq!(same.status, StatusCode::NO_CONTENT, "{region}");
        assert!(same.body.is_empty(), "{region}");
        let stale = dashboard.get(&format!("{url}&v=0000000000000000")).await;
        assert_eq!(stale.status, StatusCode::OK, "{region}");
    }
}

#[tokio::test]
async fn the_workers_version_changes_when_a_slot_fills() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let before = url_version(
        &dashboard
            .get("/fragments/workers?project=maestro")
            .await
            .body,
    );

    dashboard.apply(id, Event::Start);
    dashboard.slots.take(id, now());
    let after = url_version(
        &dashboard
            .get("/fragments/workers?project=maestro")
            .await
            .body,
    );
    assert_ne!(before, after, "a busy tile differs from an idle one");
}

#[tokio::test]
async fn a_plan_usage_poll_with_the_current_version_is_a_204() {
    let dashboard = dashboard();
    dashboard.usage.record(usage(USAGE_WITH_FABLE), REPORTED_AT);
    let region = dashboard.get("/fragments/plan-usage").await;
    assert_eq!(region.status, StatusCode::OK);
    let version = data_version(&region.body);

    let same = dashboard
        .get(&format!("/fragments/plan-usage?v={version}"))
        .await;
    assert_eq!(same.status, StatusCode::NO_CONTENT);
    assert_eq!(same.body, "");

    let stale = dashboard
        .get("/fragments/plan-usage?v=0000000000000000")
        .await;
    assert_eq!(stale.status, StatusCode::OK);
    assert!(stale.body.contains("Claude plan usage"));

    // A later report moves the "as of" time to a new minute, so it changes the
    // rendered markup and with it the version.
    dashboard
        .usage
        .record(usage(USAGE_WITH_FABLE), REPORTED_AT + 60);
    let next = data_version(&dashboard.get("/fragments/plan-usage").await.body);
    assert_ne!(version, next);
}

// --- Onboarding wizard ---

/// A dashboard with no projects yet, its onboarding checks answered by `github`.
fn empty_dashboard(github: Arc<dyn RepoChecker>) -> (Arc<Store>, Dashboard) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    seed_user_token(&store, USER);
    let dashboard = dashboard_with_github(Arc::clone(&store), github);
    (store, dashboard)
}

/// Stores a GitHub token for `login`, sealed with the key the test
/// dashboards use, so the onboarding checks have an account to run as.
fn seed_user_token(store: &Arc<Store>, login: &str) {
    seed_forge_user_token(store, login, &ForgeKind::GitHub);
}

fn seed_forge_user_token(store: &Arc<Store>, login: &str, forge: &ForgeKind) {
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(store));
    secrets
        .put_forge_token(
            ForgeTokenOwner::User { login, forge },
            &format!("token-of-{login}"),
            &ForgeAccount {
                login: format!("account-of-{login}"),
                id: 9,
            },
        )
        .unwrap();
}

#[tokio::test]
async fn the_onboard_page_is_linked_and_shows_step_one() {
    let dashboard = dashboard();
    assert!(dashboard.get("/").await.body.contains("href=\"/onboard\""));
    assert!(
        dashboard
            .settings_pages()
            .await
            .contains("href=\"/onboard\"")
    );

    let page = dashboard.get("/onboard").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Onboard a project"));
    assert!(page.body.contains("name=\"repo\""));
    // The base branch defaults to main, and only step 1 shows before a check.
    assert!(page.body.contains("value=\"main\""));
    assert!(page.body.contains("Run checks"));
    assert!(!page.body.contains("Create project"));
}

#[tokio::test]
async fn skipping_step_two_creates_a_project_with_defaults() {
    let (store, dashboard) = empty_dashboard(Arc::new(FakeRepos::fit()));
    let reply = dashboard
        .post("/onboard", "repo=acme%2Fwidgets&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/?project=widgets"));

    // The name defaults to the repository name and the base to main, with no
    // models or flag rows.
    let project = store.project("widgets").unwrap().unwrap();
    assert_eq!(project.repo, "acme/widgets");
    assert_eq!(project.base, "main");
    assert_eq!(project.checks, None);
    let none = StageModels::default();
    assert_eq!(store.project_models("widgets", &none).unwrap(), none);
    assert!(!store.project_auto_merge("widgets", false).unwrap());
    assert!(!store.project_llm_review("widgets", false).unwrap());

    // It shows on the board, the settings page and the new-job form, and a job
    // can be created against it, with no restart.
    assert!(
        dashboard
            .get("/?project=widgets")
            .await
            .body
            .contains("widgets")
    );
    assert!(dashboard.settings_pages().await.contains("widgets"));
    let created = dashboard.post_job("widgets", "A widget change", &[]).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn onboarding_makes_the_requester_the_only_collaborator() {
    let (store, dashboard) = empty_dashboard(Arc::new(FakeRepos::fit()));
    let reply = dashboard
        .post("/onboard", "repo=acme%2Fwidgets&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    let collaborators = store.collaborators("widgets").unwrap();
    assert_eq!(collaborators.len(), 1);
    assert_eq!(collaborators[0].login, "alice@example.com");
    assert_eq!(collaborators[0].added_by, "alice@example.com");
}

#[tokio::test]
async fn the_checks_run_as_the_onboarders_own_account() {
    let github = Arc::new(FakeRepos::fit());
    let seen = Arc::clone(&github.seen);
    let (store, dashboard) = empty_dashboard(github);
    seed_user_token(&store, "bob@example.com");

    let reply = dashboard
        .post_as(
            "bob@example.com",
            "/onboard",
            "repo=acme%2Fwidgets&action=check",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Some("token-of-bob@example.com".to_owned())]
    );
}

#[tokio::test]
async fn onboarding_with_the_secret_store_unavailable_stays_in_the_wizard() {
    let github = Arc::new(FakeRepos::fit());
    let seen = Arc::clone(&github.seen);
    let store = Arc::new(Store::open_in_memory().unwrap());
    let dashboard = dashboard_with_secrets(
        Arc::clone(&store),
        github,
        Arc::new(UnavailableSecrets::new(Arc::clone(&store))),
    );

    let reply = dashboard
        .post("/onboard", "repo=acme%2Fwidgets&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("secret store is unavailable"));
    assert!(reply.body.contains("href=\"/settings/forges/github\""));
    assert!(seen.lock().unwrap().is_empty());
    assert!(store.project("widgets").unwrap().is_none());
}

#[tokio::test]
async fn onboarding_without_a_token_is_blocked_and_calls_no_forge() {
    let github = Arc::new(FakeRepos::fit());
    let seen = Arc::clone(&github.seen);
    let (store, dashboard) = empty_dashboard(github);

    let reply = dashboard
        .post_as(
            "bob@example.com",
            "/onboard",
            "repo=acme%2Fwidgets&action=skip",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("href=\"/settings/forges/github\""));
    assert!(seen.lock().unwrap().is_empty());
    assert!(store.project("widgets").unwrap().is_none());
}

/// A fake Salsa checker that reports the same findings for any path and
/// records the token of each call.
#[derive(Default)]
struct FakeSalsa {
    errors: Vec<String>,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl RepoChecker for FakeSalsa {
    fn check<'a>(
        &'a self,
        account: &'a Account,
        _repo: &'a str,
        _base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        self.seen.lock().unwrap().push(account.token.clone());
        let report = RepoReport {
            errors: self.errors.clone(),
            warnings: Vec::new(),
        };
        Box::pin(std::future::ready(Ok(report)))
    }
}

/// A dashboard whose GitHub and Salsa checks are answered by separate fakes.
fn salsa_dashboard(github: Arc<FakeRepos>, salsa: Arc<FakeSalsa>) -> (Arc<Store>, Dashboard) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    seed_user_token(&store, USER);
    seed_forge_user_token(&store, USER, &ForgeKind::Salsa);
    let secrets = Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    let admins = [USER, STRANGER, MAESTRO_ONLY].map(str::to_owned).to_vec();
    let dashboard = dashboard_build(
        Arc::clone(&store),
        github,
        Some(salsa),
        |_| None,
        secrets,
        admins,
        |_| GitHub::default(),
        Vec::new(),
    );
    (store, dashboard)
}

#[tokio::test]
async fn a_salsa_project_is_checked_by_the_salsa_checker_and_stores_its_forge() {
    let github = Arc::new(FakeRepos::fit());
    let salsa = Arc::new(FakeSalsa::default());
    let github_seen = Arc::clone(&github.seen);
    let salsa_seen = Arc::clone(&salsa.seen);
    let (store, dashboard) = salsa_dashboard(github, salsa);

    let reply = dashboard
        .post("/onboard", "forge=salsa&repo=team%2Fsub%2Fpkg&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(github_seen.lock().unwrap().is_empty());
    assert_eq!(
        *salsa_seen.lock().unwrap(),
        vec![Some(format!("token-of-{USER}"))]
    );
    let project = store.project("pkg").unwrap().unwrap();
    assert_eq!(project.forge, ProjectForge::Known(ForgeKind::Salsa));
    assert_eq!(project.repo, "team/sub/pkg");
}

#[tokio::test]
async fn a_salsa_project_needs_its_renovate_account_and_a_github_project_shows_none() {
    let (store, dashboard) =
        salsa_dashboard(Arc::new(FakeRepos::fit()), Arc::new(FakeSalsa::default()));
    dashboard
        .post("/onboard", "forge=salsa&repo=team%2Fsub%2Fpkg&action=skip")
        .await;

    let page = dashboard.get("/settings/projects/pkg").await.body;
    assert!(page.contains("name=\"renovate_author\""));
    let reply = dashboard
        .post("/settings/projects/pkg/maintenance", "renovate=on")
        .await;
    assert!(reply.body.contains("Give the Renovate account"));
    let reply = dashboard
        .post(
            "/settings/projects/pkg/maintenance",
            "renovate=on&renovate_author=a%2Fb",
        )
        .await;
    assert!(reply.body.contains("must be a Salsa username"));
    assert!(!store.project_maintenance("pkg").unwrap().renovate);

    let reply = dashboard
        .post(
            "/settings/projects/pkg/maintenance",
            "renovate_author=+renovate-bot+",
        )
        .await;
    assert!(reply.body.contains("Saved."), "{}", reply.body);
    assert_eq!(
        store.project_maintenance("pkg").unwrap().renovate_author,
        Some("renovate-bot".to_owned())
    );

    let page = dashboard.get("/settings/projects/maestro").await.body;
    assert!(!page.contains("renovate_author"));
}

#[tokio::test]
async fn the_wizard_keeps_the_selected_forge_on_a_redraw() {
    let (_store, dashboard) = salsa_dashboard(
        Arc::new(FakeRepos::fit()),
        Arc::new(FakeSalsa {
            errors: vec!["no such project".into()],
            ..FakeSalsa::default()
        }),
    );
    let reply = dashboard
        .post("/onboard", "forge=salsa&repo=team%2Fpkg&action=check")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .body
            .contains("data-placeholder=\"group/subgroup/project\" checked")
    );
}

#[tokio::test]
async fn a_salsa_project_without_a_salsa_token_links_the_salsa_page() {
    let github = Arc::new(FakeRepos::fit());
    let salsa = Arc::new(FakeSalsa::default());
    let seen = Arc::clone(&salsa.seen);
    let store = Arc::new(Store::open_in_memory().unwrap());
    seed_user_token(&store, USER);
    let secrets = Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    let admins = vec![USER.to_owned()];
    let dashboard = dashboard_build(
        Arc::clone(&store),
        github,
        Some(salsa),
        |_| None,
        secrets,
        admins,
        |_| GitHub::default(),
        Vec::new(),
    );

    let reply = dashboard
        .post("/onboard", "forge=salsa&repo=team%2Fpkg&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("You have no Salsa token stored"));
    assert!(reply.body.contains("href=\"/settings/forges/salsa\""));
    assert!(seen.lock().unwrap().is_empty());
    assert!(store.project("pkg").unwrap().is_none());
}

#[tokio::test]
async fn the_same_path_onboards_once_per_forge() {
    let (store, dashboard) =
        salsa_dashboard(Arc::new(FakeRepos::fit()), Arc::new(FakeSalsa::default()));
    let github = dashboard
        .post("/onboard", "repo=team%2Fpkg&name=on-github&action=skip")
        .await;
    assert_eq!(github.status, StatusCode::SEE_OTHER);
    let salsa = dashboard
        .post(
            "/onboard",
            "forge=salsa&repo=team%2Fpkg&name=on-salsa&action=skip",
        )
        .await;
    assert_eq!(salsa.status, StatusCode::SEE_OTHER);
    let again = dashboard
        .post(
            "/onboard",
            "forge=salsa&repo=team%2Fpkg&name=again&action=skip",
        )
        .await;
    assert_eq!(again.status, StatusCode::OK);
    assert!(
        again
            .body
            .contains("The Salsa repository team/pkg is already onboarded.")
    );
    assert!(store.project("again").unwrap().is_none());
}

#[tokio::test]
async fn an_unknown_forge_is_refused_before_any_forge_call() {
    let github = Arc::new(FakeRepos::fit());
    let github_seen = Arc::clone(&github.seen);
    let salsa = Arc::new(FakeSalsa::default());
    let salsa_seen = Arc::clone(&salsa.seen);
    let (store, dashboard) = salsa_dashboard(github, salsa);
    let reply = dashboard
        .post("/onboard", "forge=gitlab&repo=acme%2Fwidgets&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("is not one the factory supports"));
    assert!(store.project("widgets").unwrap().is_none());
    assert!(github_seen.lock().unwrap().is_empty());
    assert!(salsa_seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_onboarded_project_turns_the_switch_on_and_has_no_project_token() {
    let (store, dashboard) = empty_dashboard(Arc::new(FakeRepos::fit()));
    let reply = dashboard
        .post("/onboard", "repo=acme%2Fwidgets&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    assert!(store.use_my_account("widgets", USER).unwrap());
    let project = ForgeTokenOwner::Project {
        project: "widgets",
        forge: &ForgeKind::GitHub,
    };
    assert!(store.forge_account(project).unwrap().is_none());
    let board = dashboard.get("/?project=widgets").await;
    assert!(board.body.contains("missing-project-token"));
}

#[tokio::test]
async fn step_two_values_are_stored_and_shown_on_the_settings_page() {
    let (store, dashboard) = empty_dashboard(Arc::new(FakeRepos::fit()));
    let reply = dashboard
        .post(
            "/onboard",
            "repo=acme%2Fwidgets&name=widgets&base=main&plan=opus&auto_merge=on&llm_review=on&action=create",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    assert_eq!(
        store
            .project_models("widgets", &StageModels::default())
            .unwrap()
            .plan
            .as_deref(),
        Some("opus")
    );
    assert!(store.project_auto_merge("widgets", false).unwrap());
    assert!(store.project_llm_review("widgets", false).unwrap());

    let settings = dashboard.settings_pages().await;
    assert!(settings.contains("value=\"opus\""));
    assert!(settings.contains("checked"));
}

#[tokio::test]
async fn an_optional_checks_command_is_stored() {
    let (store, dashboard) = empty_dashboard(Arc::new(FakeRepos::fit()));
    let reply = dashboard
        .post(
            "/onboard",
            "repo=acme%2Fwidgets&checks=just+lint+%26%26+just+test&action=skip",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        store.project("widgets").unwrap().unwrap().checks.as_deref(),
        Some("just lint && just test")
    );
}

#[tokio::test]
async fn warnings_do_not_block_creation() {
    // Admin, an unprotected base, no CI and no branch auto-delete are all
    // warnings.
    let github = Arc::new(FakeRepos {
        repo: Some(RepoInfo {
            permission: Permission::Admin,
            allow_squash_merge: true,
            delete_branch_on_merge: false,
        }),
        branch: Some(BranchInfo {
            protection: Protection::Unprotected,
        }),
        ci: false,
        seen: Arc::default(),
    });
    let (store, dashboard) = empty_dashboard(github);

    // The check step shows the warnings and still offers step 2.
    let checked = dashboard
        .post("/onboard", "repo=acme%2Fwidgets&action=check")
        .await;
    assert_eq!(checked.status, StatusCode::OK);
    assert!(checked.body.contains("Admin"));
    assert!(checked.body.contains("no CI"));
    assert!(checked.body.contains("Create project"));

    // Creating anyway works and writes the row.
    let reply = dashboard
        .post("/onboard", "repo=acme%2Fwidgets&action=skip")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(store.project("widgets").unwrap().is_some());
}

/// Posts an onboard create that must be rejected, and asserts the page shows
/// `needle` and no row was written.
async fn rejected(github: Arc<dyn RepoChecker>, store: Arc<Store>, form: &str, needle: &str) {
    let before = store.list_projects().unwrap().len();
    seed_user_token(&store, USER);
    let dashboard = dashboard_with_github(Arc::clone(&store), github);
    let reply = dashboard.post("/onboard", form).await;
    assert_eq!(reply.status, StatusCode::OK, "{needle}: expected a redraw");
    assert!(
        reply.body.contains(needle),
        "{needle:?} missing from the page"
    );
    assert_eq!(
        store.list_projects().unwrap().len(),
        before,
        "{needle}: a row was written"
    );
}

#[tokio::test]
async fn a_malformed_repo_is_rejected_with_no_row() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    rejected(
        Arc::new(FakeRepos::fit()),
        store,
        "repo=widgets&action=create",
        "owner/name",
    )
    .await;
}

#[tokio::test]
async fn a_missing_repo_is_rejected_with_no_row() {
    let github = Arc::new(FakeRepos {
        repo: None,
        ..FakeRepos::fit()
    });
    let store = Arc::new(Store::open_in_memory().unwrap());
    rejected(
        github,
        store,
        "repo=acme%2Fghost&action=create",
        "does not exist or is not visible",
    )
    .await;
}

#[tokio::test]
async fn a_missing_base_branch_is_rejected_with_no_row() {
    let github = Arc::new(FakeRepos {
        branch: None,
        ..FakeRepos::fit()
    });
    let store = Arc::new(Store::open_in_memory().unwrap());
    rejected(
        github,
        store,
        "repo=acme%2Fwidgets&base=trunk&action=create",
        "base branch",
    )
    .await;
}

#[tokio::test]
async fn a_bot_without_push_is_rejected_with_no_row() {
    let github = Arc::new(FakeRepos {
        repo: Some(RepoInfo {
            permission: Permission::Read,
            allow_squash_merge: true,
            delete_branch_on_merge: true,
        }),
        ..FakeRepos::fit()
    });
    let store = Arc::new(Store::open_in_memory().unwrap());
    rejected(
        github,
        store,
        "repo=acme%2Fwidgets&action=create",
        "no push access",
    )
    .await;
}

#[tokio::test]
async fn squash_merge_disabled_is_rejected_with_no_row() {
    let github = Arc::new(FakeRepos {
        repo: Some(RepoInfo {
            permission: Permission::Write,
            allow_squash_merge: false,
            delete_branch_on_merge: true,
        }),
        ..FakeRepos::fit()
    });
    let store = Arc::new(Store::open_in_memory().unwrap());
    rejected(
        github,
        store,
        "repo=acme%2Fwidgets&action=create",
        "squash merges",
    )
    .await;
}

#[tokio::test]
async fn a_duplicate_name_is_rejected_with_no_row() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "widgets",
            &ForgeKind::GitHub,
            "acme/old",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    rejected(
        Arc::new(FakeRepos::fit()),
        store,
        "repo=acme%2Fwidgets&name=widgets&action=create",
        "already exists",
    )
    .await;
}

#[tokio::test]
async fn a_duplicate_repo_is_rejected_with_no_row() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "other",
            &ForgeKind::GitHub,
            "acme/widgets",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    rejected(
        Arc::new(FakeRepos::fit()),
        store,
        "repo=acme%2Fwidgets&name=widgets&action=create",
        "already onboarded",
    )
    .await;
}

#[tokio::test]
async fn a_github_call_that_cannot_run_blocks_with_a_message() {
    // A fake whose calls fail stands in for gh being unreachable.
    #[derive(Clone)]
    struct Broken;
    impl GitHubRepos for Broken {
        fn repo(
            &self,
            _account: &Account,
            _repo: &str,
        ) -> impl Future<Output = Result<Option<RepoInfo>, String>> + Send {
            std::future::ready(Err("gh api repos/acme/widgets failed: HTTP 500".into()))
        }
        fn branch(
            &self,
            _account: &Account,
            _repo: &str,
            _branch: &str,
        ) -> impl Future<Output = Result<Option<BranchInfo>, String>> + Send {
            std::future::ready(Err("unreachable".into()))
        }
        fn has_ci(
            &self,
            _account: &Account,
            _repo: &str,
        ) -> impl Future<Output = Result<bool, String>> + Send {
            std::future::ready(Err("unreachable".into()))
        }
    }
    let store = Arc::new(Store::open_in_memory().unwrap());
    rejected(
        Arc::new(Broken),
        store,
        "repo=acme%2Fwidgets&action=create",
        "could not run",
    )
    .await;
}

#[tokio::test]
async fn a_project_onboarded_through_the_wizard_reaches_the_scheduler() {
    // A job created against a wizard-onboarded project is picked up like any
    // other: it starts in the requested stage the scheduler watches.
    let (store, dashboard) = empty_dashboard(Arc::new(FakeRepos::fit()));
    dashboard
        .post("/onboard", "repo=acme%2Fwidgets&action=skip")
        .await;
    let reply = dashboard.post_job("widgets", "A widget change", &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let id = job_id(&reply);
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::Requested);
}

#[tokio::test]
async fn an_mcp_server_is_added_edited_and_deleted_through_the_settings_page() {
    let dashboard = dashboard();

    // The empty section shows before any server is added.
    let page = dashboard.get("/settings/mcp").await;
    assert!(page.body.contains("MCP servers"));
    assert!(page.body.contains("No servers yet."));

    // Add one.
    let added = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;
    assert_eq!(added.status, StatusCode::OK);
    assert!(added.body.contains("context7"));
    assert!(added.body.contains("https://mcp.context7.com/mcp"));
    assert_eq!(
        dashboard
            .store
            .mcp_server("context7")
            .unwrap()
            .unwrap()
            .as_remote()
            .unwrap()
            .1
            .to_vec(),
        vec!["Authorization".to_owned()],
    );

    // Edit its URL path and headers. The name stays, so the route keys on it.
    let edited = dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fv2%2Fmcp&header=",
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK);
    let server = dashboard.store.mcp_server("context7").unwrap().unwrap();
    assert_eq!(
        server.as_remote().unwrap().0,
        "https://mcp.context7.com/v2/mcp"
    );
    assert_eq!(server.as_remote().unwrap().1, Vec::<String>::new());

    // Delete it.
    let deleted = dashboard
        .post("/settings/mcp/context7/delete", "confirm_name=context7")
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert!(dashboard.store.mcp_server("context7").unwrap().is_none());
}

#[tokio::test]
async fn a_stdio_mcp_server_is_added_edited_and_deleted_through_the_settings_page() {
    let dashboard = dashboard();
    let added = dashboard
        .post(
            "/settings/mcp",
            "name=playwright&kind=stdio&command=npx&arg=-y&arg=&arg=%40playwright%2Fmcp%400.0.83\
             &url=https%3A%2F%2Fignored.test%2Fmcp&header=Authorization",
        )
        .await;
    assert_eq!(added.status, StatusCode::OK);
    assert!(added.body.contains("stdio"));
    assert!(added.body.contains("@playwright/mcp@0.0.83"));
    let server = dashboard.store.mcp_server("playwright").unwrap().unwrap();
    assert_eq!(
        server.as_stdio().unwrap(),
        (
            "npx",
            &["-y".to_owned(), "@playwright/mcp@0.0.83".to_owned()][..]
        ),
    );

    let list = dashboard.get("/settings/mcp").await;
    assert!(list.body.contains("stdio: npx -y @playwright/mcp@0.0.83"));

    let edited = dashboard
        .post(
            "/settings/mcp/playwright",
            "command=uvx&arg=pkg&arg=--flag%3Da+b",
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK);
    let server = dashboard.store.mcp_server("playwright").unwrap().unwrap();
    assert_eq!(
        server.as_stdio().unwrap(),
        ("uvx", &["pkg".to_owned(), "--flag=a b".to_owned()][..]),
    );

    let deleted = dashboard
        .post("/settings/mcp/playwright/delete", "confirm_name=playwright")
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert!(dashboard.store.mcp_server("playwright").unwrap().is_none());
}

fn add_playwright(dashboard: &Dashboard) {
    dashboard
        .store
        .create_mcp_server_with(
            "playwright",
            &factory_server::McpTransport::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "@playwright/mcp@0.0.83".to_owned()],
            },
        )
        .unwrap();
}

#[tokio::test]
async fn a_stdio_mcp_server_is_not_checked() {
    let dashboard = dashboard();
    add_playwright(&dashboard);
    let check = dashboard
        .post("/settings/projects/maestro/mcp/playwright/check", "")
        .await;
    assert_eq!(check.status, StatusCode::BAD_REQUEST);
    assert_eq!(dashboard.checker.calls(), 0);
}

#[tokio::test]
async fn a_projects_page_for_a_stdio_server_has_extra_arguments_and_no_remote_parts() {
    let dashboard = dashboard();
    add_playwright(&dashboard);
    let page = dashboard
        .get("/settings/projects/maestro/mcp/playwright")
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("name=\"mcp.playwright.enabled\""));
    for stage in ["clarify", "spec", "plan", "implement", "review"] {
        assert!(
            page.body
                .contains(&format!("name=\"mcp.playwright.stage.{stage}\""))
        );
    }
    assert!(page.body.contains("Extra arguments"));
    assert!(page.body.contains("name=\"arg\""));
    assert!(page.body.contains("@playwright/mcp@0.0.83"));
    assert!(!page.body.contains("Refresh connection"));
    assert!(!page.body.contains("mcp-tools"));
    assert!(!page.body.contains(".header."));
    let project = dashboard.get("/settings/projects/maestro").await;
    assert!(project.body.contains("playwright"));
    assert!(project.body.contains("name=\"mcp.playwright.pipeline\""));
}

#[tokio::test]
async fn a_remote_servers_project_page_has_no_extra_arguments() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    let page = dashboard
        .get("/settings/projects/maestro/mcp/context7")
        .await;
    assert!(!page.body.contains("Extra arguments"));
    assert!(!page.body.contains("name=\"arg\""));
    let reply = dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.enabled=on&mcp.context7.stage.spec=on&arg=--headless&arg_escaped=a%5Cx",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        dashboard
            .store
            .project_mcp_args("maestro", "context7")
            .unwrap(),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn extra_arguments_are_saved_in_order_and_shown_in_the_command_line() {
    let dashboard = dashboard();
    add_playwright(&dashboard);
    let uri = "/settings/projects/maestro/mcp/playwright";
    let saved = dashboard
        .post(
            uri,
            "mcp.playwright.enabled=on&mcp.playwright.stage.implement=on\
             &arg=--headless&arg=&arg=--viewport-size%3D1280x720&arg=a+b",
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK);
    assert_eq!(
        dashboard
            .store
            .project_mcp_args("maestro", "playwright")
            .unwrap(),
        ["--headless", "--viewport-size=1280x720", "a b"]
    );
    let page = dashboard.get(uri).await.body;
    assert!(
        page.contains("npx -y @playwright/mcp@0.0.83 --headless --viewport-size=1280x720 'a b'")
    );

    dashboard.post(uri, "arg=--headless").await;
    assert!(
        !dashboard
            .store
            .project_mcp("maestro", "playwright")
            .unwrap()
            .enabled
    );
    assert_eq!(
        dashboard
            .store
            .project_mcp_args("maestro", "playwright")
            .unwrap(),
        ["--headless"]
    );
}

#[tokio::test]
async fn bad_extra_arguments_are_a_bad_request_that_writes_nothing() {
    let dashboard = dashboard();
    add_playwright(&dashboard);
    let uri = "/settings/projects/maestro/mcp/playwright";
    dashboard
        .post(
            uri,
            "mcp.playwright.enabled=on&mcp.playwright.stage.spec=on&arg=--keep",
        )
        .await;
    let many: String = "&arg=a".repeat(65);
    for extra in [
        many.as_str(),
        "&arg=%24%7BHOME%7D",
        "&arg=a%00b",
        "&arg_escaped=a%5Cx",
    ] {
        let reply = dashboard
            .post(
                uri,
                &format!("mcp.playwright.enabled=on&mcp.playwright.stage.plan=on{extra}"),
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{extra}");
    }
    let stored = dashboard
        .store
        .project_mcp("maestro", "playwright")
        .unwrap();
    assert!(stored.stages.spec && !stored.stages.plan);
    assert_eq!(
        dashboard
            .store
            .project_mcp_args("maestro", "playwright")
            .unwrap(),
        ["--keep"]
    );
}

#[tokio::test]
async fn a_pipeline_save_leaves_extra_arguments_alone() {
    let dashboard = dashboard();
    add_playwright(&dashboard);
    dashboard
        .post(
            "/settings/projects/maestro/mcp/playwright",
            "mcp.playwright.enabled=on&mcp.playwright.stage.spec=on&arg=--headless",
        )
        .await;
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&gates=on\
             &mcp.playwright.pipeline=on&mcp.playwright.stage.implement=on",
        )
        .await;
    let stored = dashboard
        .store
        .project_mcp("maestro", "playwright")
        .unwrap();
    assert!(stored.enabled && stored.stages.implement && !stored.stages.spec);
    assert_eq!(
        dashboard
            .store
            .project_mcp_args("maestro", "playwright")
            .unwrap(),
        ["--headless"]
    );
}

#[tokio::test]
async fn a_collaborator_of_another_project_gets_404_on_a_stdio_servers_page() {
    let dashboard = two_projects();
    add_playwright(&dashboard);
    dashboard
        .store
        .add_collaborator("tool", STRANGER, "test")
        .unwrap();
    let uri = "/settings/projects/maestro/mcp/playwright";
    assert_eq!(
        dashboard.get_as(STRANGER, uri).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dashboard
            .post_as(STRANGER, uri, "arg=--headless")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dashboard
            .store
            .project_mcp_args("maestro", "playwright")
            .unwrap(),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn an_invalid_stdio_mcp_server_is_a_bad_request() {
    let dashboard = dashboard();
    for form in [
        "name=p&kind=stdio&command=",
        "name=p&kind=stdio&command=a%0Ab",
        "name=p&kind=stdio&command=%0Anpx",
        "name=p&kind=stdio&command=npx%0D",
        "name=p&kind=stido&command=npx",
        "name=p&kind=stido&url=https%3A%2F%2Fmcp.context7.com%2Fmcp",
        "name=p&kind=stdio&command=npx&arg=%24%7BHOME%7D",
    ] {
        let reply = dashboard.post("/settings/mcp", form).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{form}");
    }
    let many = "&arg=a".repeat(65);
    let reply = dashboard
        .post(
            "/settings/mcp",
            &format!("name=p&kind=stdio&command=npx{many}"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(dashboard.store.mcp_servers().unwrap(), Vec::new());

    // An edit cannot change the kind: a remote server ignores stdio fields.
    dashboard
        .store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    let reply = dashboard
        .post(
            "/settings/mcp/context7",
            "kind=stdio&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&command=npx",
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let kept = dashboard.store.mcp_server("context7").unwrap().unwrap();
    assert_eq!(kept.as_remote().unwrap().0, "https://mcp.context7.com/mcp");
    // A stdio server refuses an explicit remote kind too.
    dashboard
        .store
        .create_mcp_server_with(
            "pw",
            &factory_server::McpTransport::Stdio {
                command: "npx".to_owned(),
                args: Vec::new(),
            },
        )
        .unwrap();
    let reply = dashboard
        .post("/settings/mcp/pw", "kind=remote&command=uvx")
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        dashboard
            .store
            .mcp_server("pw")
            .unwrap()
            .unwrap()
            .as_stdio()
            .unwrap()
            .0,
        "npx"
    );
}

#[tokio::test]
async fn a_stdio_edit_keeps_arguments_with_line_breaks() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/mcp",
            "name=pw&kind=stdio&command=npx&arg=a%0Ab&arg=c%0Dd&arg=--headless",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let stored = |dashboard: &Dashboard| {
        let server = dashboard.store.mcp_server("pw").unwrap().unwrap();
        let (command, args) = server.as_stdio().unwrap();
        (command.to_owned(), args.to_vec())
    };
    let args = vec![
        "a\nb".to_owned(),
        "c\rd".to_owned(),
        "--headless".to_owned(),
    ];
    assert_eq!(stored(&dashboard), ("npx".to_owned(), args.clone()));
    let page = dashboard.get("/settings/mcp/pw").await;
    assert_eq!(page.body.matches(r#"name="arg_escaped""#).count(), 2);
    assert!(page.body.contains(r#"value="a\nb""#));
    assert!(page.body.contains(r#"value="c\rd""#));
    let reply = dashboard
        .post(
            "/settings/mcp/pw",
            "command=uvx&arg_escaped=a%5Cnb&arg_escaped=c%5Crd&arg=--headless",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(stored(&dashboard), ("uvx".to_owned(), args));
    let reply = dashboard
        .post("/settings/mcp/pw", "command=uvx&arg_escaped=a%5Cqb")
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(stored(&dashboard).0, "uvx");
}

#[tokio::test]
async fn an_invalid_mcp_server_is_a_bad_request() {
    let dashboard = dashboard();
    // A non-https URL.
    let bad_url = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=http%3A%2F%2Fx.test%2Fmcp&header=",
        )
        .await;
    assert_eq!(bad_url.status, StatusCode::BAD_REQUEST);
    // A reserved header name.
    let reserved = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fx.test%2Fmcp&header=Host",
        )
        .await;
    assert_eq!(reserved.status, StatusCode::BAD_REQUEST);
    // Nothing was stored.
    assert_eq!(dashboard.store.mcp_servers().unwrap(), Vec::new());
}

#[tokio::test]
async fn the_add_server_form_has_header_name_rows_not_a_textarea() {
    let dashboard = dashboard();
    dashboard
        .store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    let page = dashboard.settings_pages().await;
    // The header names live in text boxes posting a repeated `header` field,
    // with an "Add header" button, and no textarea or `headers` field remains.
    assert!(page.contains(r#"name="header""#));
    assert!(page.contains("Add header"));
    // The edit form's header-name pattern escapes `|` and the trailing `-`, as
    // the `v` flag a browser compiles it with requires.
    assert!(page.contains(r"\|~\-]+"));
    assert!(!page.contains(r#"name="headers""#));
    assert!(!page.contains("<textarea id=\"mcp-new-headers\""));
}

#[tokio::test]
async fn header_rows_are_trimmed_deduplicated_and_empty_rows_dropped() {
    let dashboard = dashboard();
    // One empty row, a duplicate spelling and a padded value all collapse: the
    // server stores "Authorization" once and "X-Team" once.
    let added = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp\
             &header=Authorization&header=&header=+authorization+&header=X-Team",
        )
        .await;
    assert_eq!(added.status, StatusCode::OK);
    assert_eq!(
        dashboard
            .store
            .mcp_server("context7")
            .unwrap()
            .unwrap()
            .as_remote()
            .unwrap()
            .1
            .to_vec(),
        vec!["Authorization".to_owned(), "X-Team".to_owned()],
    );
}

#[tokio::test]
async fn a_header_name_with_a_colon_is_a_bad_request() {
    let dashboard = dashboard();
    // A pasted "Authorization: Bearer x" that slips past the browser pattern is
    // refused by the server, and nothing is stored.
    let bad = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fx.test%2Fmcp&header=Authorization%3A+Bearer+x",
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(dashboard.store.mcp_servers().unwrap(), Vec::new());
}

#[tokio::test]
async fn an_edit_form_shows_a_filled_row_per_name_and_an_empty_one() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;
    let page = dashboard.settings_pages().await;
    // The edit form carries the stored name in a text box, ready to edit.
    assert!(page.contains(r#"value="Authorization""#));
    assert!(page.contains(r#"name="header""#));
    // Clearing the one row on save removes the header.
    let edited = dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=",
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK);
    assert_eq!(
        dashboard
            .store
            .mcp_server("context7")
            .unwrap()
            .unwrap()
            .as_remote()
            .unwrap()
            .1
            .to_vec(),
        Vec::<String>::new(),
    );
}

/// Stores a value for maestro's `context7` `Authorization` header with the same
/// key the dashboard's secret store uses, so the dashboard reads it back.
fn store_auth_value(dashboard: &Dashboard, value: &str) {
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    secrets
        .put(
            factory_server::SecretRef {
                project: "maestro",
                server: "context7",
                header: "Authorization",
            },
            value,
        )
        .unwrap();
}

fn stored_auth_value(dashboard: &Dashboard) -> Option<String> {
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    secrets
        .get(factory_server::SecretRef {
            project: "maestro",
            server: "context7",
            header: "Authorization",
        })
        .unwrap()
}

#[tokio::test]
async fn a_projects_form_has_a_block_per_server_defaulting_to_off() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;
    let page = dashboard.settings_pages().await;
    // The project form carries the server's enabled switch, a checkbox per
    // stage, and the declared header as a write-only field showing "not set".
    assert!(page.contains("name=\"mcp.context7.enabled\""));
    for stage in ["clarify", "spec", "plan", "implement", "review"] {
        assert!(
            page.contains(&format!("name=\"mcp.context7.stage.{stage}\"")),
            "missing the {stage} checkbox",
        );
    }
    assert!(page.contains("name=\"mcp.context7.header.Authorization\""));
    assert!(page.contains("name=\"mcp.context7.clear.Authorization\""));
    assert!(page.contains("placeholder=\"not set\""));
    // Off by default: the enabled box is not checked and no stage is.
    assert_eq!(
        dashboard.store.project_mcp("maestro", "context7").unwrap(),
        factory_server::McpProjectSettings::default(),
    );
}

#[tokio::test]
async fn enabling_a_server_with_no_stage_is_a_bad_request() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=",
        )
        .await;
    let reply = dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=&mcp.context7.enabled=on",
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("no stage"));
    // Nothing was stored for the server.
    assert_eq!(
        dashboard.store.project_mcp("maestro", "context7").unwrap(),
        factory_server::McpProjectSettings::default(),
    );
}

#[tokio::test]
async fn enabling_a_server_with_a_stage_saves_it() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=",
        )
        .await;
    let reply = dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.clarify=on",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let saved = dashboard.store.project_mcp("maestro", "context7").unwrap();
    assert!(saved.enabled);
    assert!(saved.stages.clarify);
    assert!(!saved.stages.review);
}

#[tokio::test]
async fn the_page_never_renders_a_stored_header_value() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;
    store_auth_value(&dashboard, "Bearer sk-secret-xyz");

    let page = dashboard.settings_pages().await;
    assert!(
        !page.contains("sk-secret-xyz"),
        "the stored value leaked into the HTML"
    );
    // The field shows only that a value is stored.
    assert!(page.contains("placeholder=\"set\""));
}

#[tokio::test]
async fn an_empty_header_field_keeps_the_stored_value_and_clear_deletes_it() {
    let dashboard = dashboard();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;
    store_auth_value(&dashboard, "Bearer sk-keep");

    // Saving with the header field empty leaves the stored value untouched.
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.clarify=on&mcp.context7.header.Authorization=",
        )
        .await;
    assert_eq!(
        stored_auth_value(&dashboard).as_deref(),
        Some("Bearer sk-keep")
    );

    // Typing a new value replaces it.
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.clarify=on\
             &mcp.context7.header.Authorization=Bearer%20sk-new",
        )
        .await;
    assert_eq!(
        stored_auth_value(&dashboard).as_deref(),
        Some("Bearer sk-new")
    );

    // The clear control deletes the stored value.
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.clarify=on\
             &mcp.context7.clear.Authorization=on",
        )
        .await;
    assert_eq!(stored_auth_value(&dashboard), None);
}

#[tokio::test]
async fn saving_one_projects_mcp_leaves_other_projects_alone() {
    let dashboard = two_projects();
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=",
        )
        .await;
    // tool turns the server on for the plan stage.
    dashboard
        .post(
            "/settings/projects/tool/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.plan=on",
        )
        .await;
    // Saving maestro's MCP settings does not touch tool's.
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.review=on",
        )
        .await;
    let tool = dashboard.store.project_mcp("tool", "context7").unwrap();
    assert!(tool.enabled && tool.stages.plan && !tool.stages.review);
    let maestro = dashboard.store.project_mcp("maestro", "context7").unwrap();
    assert!(maestro.enabled && maestro.stages.review && !maestro.stages.plan);
}

#[tokio::test]
async fn without_a_master_key_the_page_reports_secrets_off_and_refuses_a_value() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with_secrets(
        Arc::clone(&store),
        Arc::new(FakeRepos::fit()),
        Arc::new(UnavailableSecrets::new(Arc::clone(&store))),
    );
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;

    // The page says the master key is missing.
    let page = dashboard.settings_pages().await;
    assert!(page.contains("master key is missing"));

    // A header value cannot be saved, but the enabled and stage choices still can.
    let refused = dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.clarify=on\
             &mcp.context7.header.Authorization=Bearer%20sk-x",
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(refused.body.contains("unavailable"));
    // The refused save wrote nothing, since the check runs before any write.
    assert_eq!(
        store.project_mcp("maestro", "context7").unwrap(),
        factory_server::McpProjectSettings::default(),
    );

    // Without a value field, the enabled and stage choices save.
    let ok = dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "clarify=&spec=&plan=&implement=&review=\
             &mcp.context7.enabled=on&mcp.context7.stage.clarify=on",
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK);
    assert!(store.project_mcp("maestro", "context7").unwrap().enabled);
}

/// The Claude token stored with the same key the dashboard's secret store uses.
fn stored_claude_token(dashboard: &Dashboard) -> Option<String> {
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .claude_token()
        .unwrap()
}

/// Stores a Claude token with the same key the dashboard's secret store uses.
fn store_claude_token(dashboard: &Dashboard, value: &str) {
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .set_claude_token(value)
        .unwrap();
}

#[tokio::test]
async fn the_claude_agent_page_has_the_token_card_under_a_claude_heading() {
    let dashboard = dashboard();
    let page = dashboard.get("/settings/agents/claude").await.body;
    assert!(page.contains("<h1>Claude</h1>"));
    assert!(page.contains("Claude token"));
    assert!(page.contains("name=\"claude_token\""));
    assert!(page.contains("action=\"/settings/agents/claude/token\""));
    // No token yet, so the field shows "not set".
    assert!(page.contains("placeholder=\"not set\""));
}

#[tokio::test]
async fn the_claude_token_is_never_rendered_but_its_presence_shows() {
    let dashboard = dashboard();
    store_claude_token(&dashboard, "sk-ant-oat-secret-xyz");

    let page = dashboard.settings_pages().await;
    assert!(
        !page.contains("sk-ant-oat-secret-xyz"),
        "the stored token leaked into the HTML"
    );
    // The field shows only that a token is stored, never a value attribute.
    assert!(page.contains("placeholder=\"set\""));
    assert!(!page.contains("value=\"sk-ant"));
}

#[tokio::test]
async fn saving_the_claude_token_replaces_it_and_trims_whitespace() {
    let dashboard = dashboard();
    store_claude_token(&dashboard, "sk-ant-oat-old");

    // A value pasted with surrounding whitespace and a trailing newline is
    // stored without them, and replaces the old token.
    let reply = dashboard
        .post(
            "/settings/agents/claude/token",
            "claude_token=%20sk-ant-oat-new%0A",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    assert_eq!(
        stored_claude_token(&dashboard).as_deref(),
        Some("sk-ant-oat-new")
    );
}

#[tokio::test]
async fn an_empty_claude_token_field_keeps_the_stored_token() {
    let dashboard = dashboard();
    store_claude_token(&dashboard, "sk-ant-oat-keep");

    let reply = dashboard
        .post("/settings/agents/claude/token", "claude_token=")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        stored_claude_token(&dashboard).as_deref(),
        Some("sk-ant-oat-keep")
    );
}

#[tokio::test]
async fn the_clear_control_removes_the_claude_token() {
    let dashboard = dashboard();
    store_claude_token(&dashboard, "sk-ant-oat-gone");

    let reply = dashboard
        .post("/settings/agents/claude/token/clear", "claude_token=")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Cleared."));
    assert_eq!(stored_claude_token(&dashboard), None);
}

#[tokio::test]
async fn without_a_master_key_the_claude_token_field_is_disabled_with_a_message() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let dashboard = dashboard_with_secrets(
        Arc::clone(&store),
        Arc::new(FakeRepos::fit()),
        Arc::new(UnavailableSecrets::new(Arc::clone(&store))),
    );

    let page = dashboard.settings_pages().await;
    // The field is present but disabled, and the page says why.
    assert!(page.contains("name=\"claude_token\""));
    assert!(page.contains("master key is missing"));
    assert!(page.contains("CLAUDE_CODE_OAUTH_TOKEN"));
    // The token input is one of the disabled fields.
    assert!(page.contains("disabled"));
}

/// Adds the `context7` server with an `Authorization` header through the form.
async fn add_context7(dashboard: &Dashboard) {
    dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=Authorization",
        )
        .await;
}

/// Stores `value` for `project`'s `context7` `Authorization` header with the
/// same key the dashboard's secret store uses.
fn store_value_for(dashboard: &Dashboard, project: &str, value: &str) {
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    secrets
        .put(
            factory_server::SecretRef {
                project,
                server: "context7",
                header: "Authorization",
            },
            value,
        )
        .unwrap();
}

#[tokio::test]
async fn a_project_server_block_has_a_refresh_connection_button() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    let page = dashboard.settings_pages().await;
    assert!(page.contains("Refresh connection"));
    assert!(page.contains(r#"hx-post="/settings/projects/maestro/mcp/context7/check""#));
    assert!(page.contains(r#"hx-post="/settings/projects/tool/mcp/context7/check""#));
    // The reply replaces the button's own block, found from the button, so two
    // projects' blocks for one server never swap into each other.
    assert!(page.contains(r#"hx-target="closest .mcp-refresh""#));
    assert!(page.contains("No tools listed yet"));
}

fn tool(name: &str, description: Option<&str>) -> McpTool {
    McpTool {
        name: name.to_owned(),
        description: description.map(str::to_owned),
    }
}

fn listed(tools: &[&str]) -> CheckOutcome {
    CheckOutcome::Ok {
        server_name: None,
        server_version: None,
        protocol_version: "2025-06-18".to_owned(),
        tools: ToolList::Listed(tools.iter().map(|name| tool(name, None)).collect()),
    }
}

async fn refresh(dashboard: &Dashboard, project: &str) -> Reply {
    dashboard
        .post(
            &format!("/settings/projects/{project}/mcp/context7/check"),
            "",
        )
        .await
}

/// The tool states `project` has for `context7`, as `(name, offered, disabled, new)`.
fn tool_states(dashboard: &Dashboard, project: &str) -> Vec<(String, bool, bool, bool)> {
    dashboard
        .store
        .mcp_tools(project, "context7")
        .unwrap()
        .into_iter()
        .map(|tool| (tool.name, tool.offered, tool.disabled, tool.new))
        .collect()
}

#[tokio::test]
async fn a_refresh_stores_the_listed_tools_and_shows_them_on() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard.checker.set(CheckOutcome::Ok {
        server_name: None,
        server_version: None,
        protocol_version: "2025-06-18".to_owned(),
        tools: ToolList::Listed(vec![
            tool("resolve-library-id", Some("Finds a <library> id")),
            tool("query-docs", None),
        ]),
    });

    let reply = refresh(&dashboard, "maestro").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Listed 2 tools."));
    assert!(reply.body.contains(r#"class="mcp-refresh""#));
    assert!(
        reply
            .body
            .contains(r#"name="mcp.context7.tool.query-docs" value="on" checked"#)
    );
    assert!(
        reply
            .body
            .contains(r#"name="mcp.context7.shown.resolve-library-id""#)
    );
    assert!(reply.body.contains("Finds a &lt;library&gt; id"));
    // The first refresh marks nothing new.
    assert!(!reply.body.contains("(new)"));
    assert_eq!(
        tool_states(&dashboard, "maestro"),
        vec![
            ("query-docs".to_owned(), true, false, false),
            ("resolve-library-id".to_owned(), true, false, false),
        ],
    );
    // The tools belong to the project that refreshed.
    assert_eq!(tool_states(&dashboard, "other"), Vec::new());
    let page = dashboard.settings_pages().await;
    assert!(page.contains(r#"name="mcp.context7.tool.resolve-library-id" value="on" checked"#));
}

#[tokio::test]
async fn saving_turns_off_the_shown_tools_left_unchecked() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard.checker.set(listed(&["a", "b", "c"]));
    refresh(&dashboard, "maestro").await;

    // `a` is shown and checked, `b` is shown and unchecked, `c` is not shown.
    let reply = dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.shown.a=on&mcp.context7.tool.a=on&mcp.context7.shown.b=on",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        tool_states(&dashboard, "maestro"),
        vec![
            ("a".to_owned(), true, false, false),
            ("b".to_owned(), true, true, false),
            ("c".to_owned(), true, false, false),
        ],
    );
    let page = dashboard.settings_pages().await;
    assert!(page.contains(r#"name="mcp.context7.tool.b" value="on">"#));

    // Checking it again turns it back on.
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.shown.b=on&mcp.context7.tool.b=on",
        )
        .await;
    assert!(!tool_states(&dashboard, "maestro")[1].2);
}

#[tokio::test]
async fn a_later_refresh_marks_new_tools_and_keeps_a_dropped_one_off() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard.checker.set(listed(&["a", "b"]));
    refresh(&dashboard, "maestro").await;
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.shown.a=on&mcp.context7.shown.b=on&mcp.context7.tool.b=on",
        )
        .await;

    // The server drops `a` and adds `c`.
    dashboard.checker.set(listed(&["b", "c"]));
    let reply = refresh(&dashboard, "maestro").await;
    assert!(
        reply
            .body
            .contains(r#"<span class="badge agent">new</span>"#)
    );
    assert!(
        reply
            .body
            .contains(r#"<span class="badge factory">no longer offered</span>"#)
    );
    assert_eq!(
        tool_states(&dashboard, "maestro"),
        vec![
            ("a".to_owned(), false, true, false),
            ("b".to_owned(), true, false, false),
            ("c".to_owned(), true, false, true),
        ],
    );

    // Offered again, `a` is still off.
    dashboard.checker.set(listed(&["a", "b", "c"]));
    refresh(&dashboard, "maestro").await;
    assert_eq!(
        tool_states(&dashboard, "maestro"),
        vec![
            ("a".to_owned(), true, true, false),
            ("b".to_owned(), true, false, false),
            ("c".to_owned(), true, false, false),
        ],
    );
}

#[tokio::test]
async fn a_failed_listing_keeps_the_stored_tools() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard.checker.set(listed(&["a"]));
    refresh(&dashboard, "maestro").await;

    dashboard.checker.set(CheckOutcome::Ok {
        server_name: None,
        server_version: None,
        protocol_version: "2025-06-18".to_owned(),
        tools: ToolList::Failed("the server did not list its tools in time".to_owned()),
    });
    let reply = refresh(&dashboard, "maestro").await;
    assert!(reply.body.contains("Could not list the tools"));
    assert!(reply.body.contains(r#"name="mcp.context7.tool.a""#));
    assert_eq!(
        tool_states(&dashboard, "maestro"),
        vec![("a".to_owned(), true, false, false)],
    );

    // A failed connection keeps them too.
    dashboard.checker.set(CheckOutcome::Unreachable);
    refresh(&dashboard, "maestro").await;
    assert_eq!(tool_states(&dashboard, "maestro").len(), 1);
}

#[tokio::test]
async fn an_origin_change_unlists_the_tools_and_a_delete_drops_them() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard.checker.set(listed(&["a", "b"]));
    refresh(&dashboard, "maestro").await;
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.shown.a=on&mcp.context7.shown.b=on&mcp.context7.tool.b=on",
        )
        .await;

    // A path change keeps the list as it is.
    dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fv2&header=Authorization",
        )
        .await;
    assert!(tool_states(&dashboard, "maestro").iter().all(|t| t.1));

    // A new host marks every tool not offered and keeps `a` off.
    dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fevil.example%2Fmcp&header=Authorization",
        )
        .await;
    assert_eq!(
        tool_states(&dashboard, "maestro"),
        vec![
            ("a".to_owned(), false, true, false),
            ("b".to_owned(), false, false, false),
        ],
    );

    dashboard
        .post("/settings/mcp/context7/delete", "confirm_name=context7")
        .await;
    assert_eq!(tool_states(&dashboard, "maestro"), Vec::new());
}

#[tokio::test]
async fn a_successful_check_shows_connected_and_sends_the_stored_value() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-stored-xyz");
    dashboard.checker.set(CheckOutcome::Ok {
        server_name: Some("context7".to_owned()),
        server_version: Some("1.2.3".to_owned()),
        protocol_version: "2025-06-18".to_owned(),
        tools: ToolList::Listed(Vec::new()),
    });

    let reply = dashboard
        .post("/settings/projects/maestro/mcp/context7/check", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Connected"));
    assert!(reply.body.contains("context7"));
    assert!(reply.body.contains("1.2.3"));
    assert!(reply.body.contains("2025-06-18"));
    // The project's stored value reached the checker, and never the page.
    assert_eq!(
        dashboard.checker.last_headers(),
        vec![(
            "Authorization".to_owned(),
            "Bearer sk-stored-xyz".to_owned()
        )],
    );
    assert!(
        !reply.body.contains("sk-stored-xyz"),
        "the stored value leaked into the fragment",
    );
}

#[tokio::test]
async fn each_projects_check_sends_that_projects_value() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-maestro");
    store_value_for(&dashboard, "tool", "Bearer sk-tool");
    dashboard.checker.set(CheckOutcome::Ok {
        server_name: None,
        server_version: None,
        protocol_version: "2025-06-18".to_owned(),
        tools: ToolList::Listed(Vec::new()),
    });

    dashboard
        .post("/settings/projects/maestro/mcp/context7/check", "")
        .await;
    assert_eq!(
        dashboard.checker.last_headers(),
        vec![("Authorization".to_owned(), "Bearer sk-maestro".to_owned())],
    );
    dashboard
        .post("/settings/projects/tool/mcp/context7/check", "")
        .await;
    assert_eq!(
        dashboard.checker.last_headers(),
        vec![("Authorization".to_owned(), "Bearer sk-tool".to_owned())],
    );
}

#[tokio::test]
async fn the_check_ignores_an_unsaved_value_in_the_form() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-saved");
    dashboard.checker.set(CheckOutcome::Ok {
        server_name: None,
        server_version: None,
        protocol_version: "2025-06-18".to_owned(),
        tools: ToolList::Listed(Vec::new()),
    });

    // The form posts a different, unsaved value; the handler ignores it and the
    // checker still gets the saved one.
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7/check",
            "mcp.context7.header.Authorization=Bearer%20sk-typed",
        )
        .await;
    assert_eq!(
        dashboard.checker.last_headers(),
        vec![("Authorization".to_owned(), "Bearer sk-saved".to_owned())],
    );
}

#[tokio::test]
async fn a_refused_check_names_a_header_with_no_stored_value() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    // No value is stored for Authorization.
    dashboard.checker.set(CheckOutcome::Refused { status: 401 });

    let reply = dashboard
        .post("/settings/projects/maestro/mcp/context7/check", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("refused the request"));
    assert!(reply.body.contains("401"));
    assert!(reply.body.contains("No value stored for: Authorization"));
    // The check ran with no header, since none is stored.
    assert_eq!(dashboard.checker.last_headers(), Vec::new());
}

#[tokio::test]
async fn a_json_rpc_error_shows_the_servers_message_escaped() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard.checker.set(CheckOutcome::RpcError {
        code: -32603,
        message: "clientInfo.version <b>missing</b>".to_owned(),
    });

    let reply = dashboard
        .post("/settings/projects/maestro/mcp/context7/check", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("JSON-RPC error -32603"));
    assert!(
        reply
            .body
            .contains("clientInfo.version &lt;b&gt;missing&lt;/b&gt;")
    );
}

#[tokio::test]
async fn the_check_route_404s_for_an_unknown_project_or_server() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;

    let unknown_project = dashboard
        .post("/settings/projects/nope/mcp/context7/check", "")
        .await;
    assert_eq!(unknown_project.status, StatusCode::NOT_FOUND);

    let unknown_server = dashboard
        .post("/settings/projects/maestro/mcp/absent/check", "")
        .await;
    assert_eq!(unknown_server.status, StatusCode::NOT_FOUND);
    // Neither 404 ran a check.
    assert_eq!(dashboard.checker.calls(), 0);
}

#[tokio::test]
async fn settings_opens_the_first_own_project_or_onboarding_without_one() {
    let reply = two_projects().get("/settings").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some("/settings/projects/maestro")
    );

    let empty = dashboard_with(Arc::new(Store::open_in_memory().unwrap()));
    let reply = empty.get("/settings").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/settings/notifications"));
}

#[tokio::test]
async fn a_login_with_no_user_settings_row_has_notifications_on() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.notifications_enabled("nobody@example.com").unwrap());
    store.set_notifications("a@x", false).unwrap();
    assert!(!store.notifications_enabled("a@x").unwrap());
    assert!(store.notifications_enabled("b@y").unwrap());
    store.set_notifications("a@x", true).unwrap();
    assert!(store.notifications_enabled("a@x").unwrap());
}

#[tokio::test]
async fn the_notifications_page_saves_the_setting_for_the_current_login_only() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    let uri = "/settings/notifications";
    let page = dashboard.get_as("bob@example.com", uri).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Browser notifications for my jobs"));
    assert!(page.body.contains("name=\"notifications\""));
    assert!(page.body.contains("checked"));

    // An unticked checkbox posts nothing.
    let saved = dashboard.post_as("bob@example.com", uri, "").await;
    assert_eq!(saved.status, StatusCode::OK);
    assert!(saved.body.contains("Saved."));
    assert!(
        !dashboard
            .store
            .notifications_enabled("bob@example.com")
            .unwrap()
    );
    assert!(dashboard.store.notifications_enabled(USER).unwrap());
    let page = dashboard.get_as("bob@example.com", uri).await;
    assert!(!page.body.contains("checked"));

    dashboard
        .post_as("bob@example.com", uri, "notifications=on")
        .await;
    assert!(
        dashboard
            .store
            .notifications_enabled("bob@example.com")
            .unwrap()
    );
}

#[tokio::test]
async fn the_notifications_page_opens_for_a_non_admin_with_only_the_you_and_projects_groups() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    let body = dashboard
        .get_as("nobody@example.com", "/settings/notifications")
        .await
        .body;
    assert!(body.contains(">You<"));
    assert!(body.contains("aria-current=\"page\""));
    assert!(body.contains("id=\"notification-permission\""));
    for hidden in [
        "Integrations",
        "Default gates",
        ">Agents<",
        "/settings/workers",
    ] {
        assert!(!body.contains(hidden), "{hidden}");
    }
}

#[tokio::test]
async fn the_settings_sidebar_links_every_page_and_marks_the_open_one() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    let body = dashboard.get("/settings/polling").await.body;
    for href in [
        "/settings/workers",
        "/settings/gates",
        "/settings/polling",
        "/settings/agents/claude",
        "/settings/mcp",
        "/settings/projects/maestro",
        "/settings/projects/tool",
        "/onboard",
    ] {
        assert!(body.contains(&format!("href=\"{href}\"")), "{href}");
    }
    assert!(body.contains(r#"href="/settings/polling" aria-current="page""#));
    assert_eq!(body.matches(r#"" aria-current="page""#).count(), 1);
    assert!(body.contains("querySelector(\".settings-nav\")"));

    // A server's page marks the MCP servers entry.
    let body = dashboard.get("/settings/mcp/context7").await.body;
    assert!(body.contains(r#"href="/settings/mcp" aria-current="page""#));
}

#[tokio::test]
async fn the_sidebar_groups_agents_and_links_each_one() {
    let dashboard = dashboard();
    let body = dashboard.get("/settings/polling").await.body;

    // The sidebar has an Agents group between Factory and Integrations.
    assert!(body.contains("Agents"));
    let agents = body.find("Agents").unwrap();
    let integrations = body.find("Integrations").unwrap();
    assert!(agents < integrations);

    // Claude links to its page; "Claude account" is gone from Factory.
    assert!(body.contains(r#"href="/settings/agents/claude""#));
    assert!(!body.contains("Claude account"));

    assert!(body.contains("Codex"));
    assert!(body.contains(r#"href="/settings/agents/codex""#));

    assert!(body.contains("Copilot"));
    assert!(body.contains(r#"href="/settings/agents/copilot""#));

    let local = body.find(">Local</a>").unwrap();
    assert!(body.find("Copilot").unwrap() < local);
    assert!(body.contains(r#"href="/settings/agents/local""#));

    // The Claude agent page marks its own sidebar entry.
    let body = dashboard.get("/settings/agents/claude").await.body;
    assert!(body.contains(r#"href="/settings/agents/claude" aria-current="page""#));

    let body = dashboard.get("/settings/agents/copilot").await.body;
    assert!(body.contains(r#"href="/settings/agents/copilot" aria-current="page""#));
}

#[tokio::test]
async fn the_old_claude_url_redirects_to_the_agent_page() {
    let dashboard = dashboard();
    let reply = dashboard.get("/settings/claude").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/settings/agents/claude"));
}

#[tokio::test]
async fn the_old_github_url_redirects_to_the_polling_page() {
    let dashboard = dashboard();
    let reply = dashboard.get("/settings/github").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/settings/polling"));

    // A form opened before the move posts to the old path and still saves.
    let reply = dashboard.post("/settings/github", "poll_seconds=30").await;
    assert_eq!(reply.status, StatusCode::TEMPORARY_REDIRECT);
    let target = reply.location.unwrap();
    assert_eq!(target, "/settings/polling");
    let reply = dashboard.post(&target, "poll_seconds=30").await;
    assert!(reply.body.contains("Saved."));
    assert_eq!(dashboard.store.settings().unwrap().poll_seconds, 30);
}

#[tokio::test]
async fn each_settings_form_ends_in_a_save_bar_and_the_page_tracks_changes() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    for uri in [
        "/settings/workers",
        "/settings/gates",
        "/settings/polling",
        "/settings/agents/claude",
        "/settings/mcp/context7",
        "/settings/projects/maestro",
    ] {
        let body = dashboard.get(uri).await.body;
        let bars = if uri == "/settings/projects/maestro" {
            8
        } else {
            1
        };
        assert_eq!(body.matches(r#"class="save-bar""#).count(), bars, "{uri}");
        assert!(body.contains(r#"class="card settings-form"#), "{uri}");
        assert!(body.contains("beforeunload"), "{uri}");
    }
}

#[tokio::test]
async fn save_comes_before_delete_and_clear_so_enter_saves() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    let body = dashboard.get("/settings/mcp/context7").await.body;
    let save = body
        .find("<button class=\"primary\">Save</button>")
        .unwrap();
    let delete = body.find("/settings/mcp/context7/delete").unwrap();
    assert!(save < delete);

    let body = dashboard.get("/settings/agents/claude").await.body;
    let save = body.find("Save token").unwrap();
    let clear = body.find("Clear token").unwrap();
    assert!(save < clear);
}

#[tokio::test]
async fn each_factory_page_saves_only_its_own_fields() {
    let dashboard = dashboard();
    dashboard.post("/settings/gates", "gate_publish=on").await;
    dashboard.post("/settings/polling", "poll_seconds=90").await;
    dashboard
        .post(
            "/settings/workers",
            "max_workers=3&worker_image=i&worker_memory=3g&worker_cpus=2",
        )
        .await;
    let settings = dashboard.store.settings().unwrap();
    assert_eq!(settings.max_workers, 3);
    assert_eq!(settings.poll_seconds, 90);
    assert_eq!(
        settings.gates,
        Gates {
            spec: false,
            plan: false,
            publish: true,
        }
    );
}

#[tokio::test]
async fn a_save_redraws_the_same_page_with_a_notice() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    assert!(reply.body.contains("<h1>maestro</h1>"));

    let reply = dashboard.post("/settings/gates", "").await;
    assert!(reply.body.contains("Saved."));
    assert!(reply.body.contains("<h1>Default gates</h1>"));

    add_context7(&dashboard).await;
    let reply = dashboard
        .post("/settings/mcp/context7/delete", "confirm_name=context7")
        .await;
    assert!(reply.body.contains("Deleted."));
    assert!(reply.body.contains("<h1>MCP servers</h1>"));
}

#[tokio::test]
async fn adding_a_server_opens_its_page() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Added."));
    assert!(reply.body.contains("<h1>context7</h1>"));
    let list = dashboard.get("/settings/mcp").await.body;
    assert!(list.contains(r#"href="/settings/mcp/context7""#));
}

#[tokio::test]
async fn an_unknown_project_or_server_page_is_not_found() {
    let dashboard = dashboard();
    assert_eq!(
        dashboard.get("/settings/projects/nope").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dashboard.get("/settings/mcp/nope").await.status,
        StatusCode::NOT_FOUND
    );
}

fn has_value(dashboard: &Dashboard, project: &str, header: &str) -> bool {
    dashboard
        .store
        .has_secret(factory_server::SecretRef {
            project,
            server: "context7",
            header,
        })
        .unwrap()
}

#[tokio::test]
async fn a_project_page_sums_up_each_server_and_links_its_page() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-x");
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.enabled=on&mcp.context7.stage.plan=on&mcp.context7.stage.review=on",
        )
        .await;

    let page = dashboard.get("/settings/projects/maestro").await.body;
    assert!(page.contains("On in 2 of 5 stages"));
    assert!(page.contains("Key stored"));
    assert!(page.contains(r#"href="/settings/projects/maestro/mcp/context7""#));
    // The project form carries the stage switches, not the server's other
    // fields.
    assert!(!page.contains("mcp.context7.enabled"));
    assert!(!page.contains("mcp.context7.header"));

    let other = dashboard.get("/settings/projects/tool").await.body;
    assert!(other.contains("Off"));
    assert!(other.contains("Key missing"));
}

#[tokio::test]
async fn saving_the_project_form_leaves_its_mcp_settings_alone() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.enabled=on&mcp.context7.stage.plan=on",
        )
        .await;
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=opus&plan=&implement=&review=",
        )
        .await;
    assert!(
        dashboard
            .store
            .project_mcp("maestro", "context7")
            .unwrap()
            .enabled
    );
}

#[tokio::test]
async fn a_projects_server_page_holds_its_settings_and_the_tool_controls() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard
        .checker
        .set(listed(&["query-docs", "resolve-library-id"]));
    refresh(&dashboard, "maestro").await;

    let page = dashboard
        .get("/settings/projects/maestro/mcp/context7")
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let body = page.body;
    assert!(body.contains("<h1>context7 for maestro</h1>"));
    assert!(body.contains(r#"action="/settings/projects/maestro/mcp/context7""#));
    assert!(body.contains(r#"href="/settings/projects/maestro" aria-current="page""#));
    assert!(body.contains("Refresh connection"));
    assert!(body.contains(r#"class="mcp-tool-controls""#));
    assert!(body.contains("2 of 2 on"));
    assert!(body.contains(r#"data-tool="query-docs""#));
    assert!(body.contains("mcp-tools-js"));
    assert_eq!(body.matches(r#"class="save-bar""#).count(), 1);
}

#[tokio::test]
async fn a_projects_server_page_for_an_unknown_name_is_not_found() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    for uri in [
        "/settings/projects/nope/mcp/context7",
        "/settings/projects/maestro/mcp/nope",
    ] {
        assert_eq!(
            dashboard.get(uri).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
        assert_eq!(
            dashboard.post(uri, "").await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn a_servers_page_lists_the_projects_that_use_it() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-x");
    dashboard
        .post(
            "/settings/projects/maestro/mcp/context7",
            "mcp.context7.enabled=on&mcp.context7.stage.clarify=on",
        )
        .await;

    let page = dashboard.get("/settings/mcp/context7").await.body;
    let used = &page[page.find("Used by").unwrap()..];
    assert!(used.contains("maestro"));
    assert!(used.contains("On in 1 of 5 stages"));
    assert!(used.contains("Key stored"));
    assert!(used.contains("tool"));
    assert!(used.contains("Key missing"));
    assert!(used.contains(r#"href="/settings/projects/tool/mcp/context7""#));
}

#[tokio::test]
async fn a_servers_usage_card_lists_only_the_logins_own_projects() {
    let dashboard = two_projects();
    dashboard
        .store
        .add_collaborator("maestro", MAESTRO_ONLY, "test")
        .unwrap();
    add_context7(&dashboard).await;
    for project in ["maestro", "tool"] {
        store_value_for(&dashboard, project, "Bearer sk-x");
        dashboard
            .post(
                &format!("/settings/projects/{project}/mcp/context7"),
                "mcp.context7.enabled=on&mcp.context7.stage.clarify=on",
            )
            .await;
    }

    let page = dashboard
        .get_as(MAESTRO_ONLY, "/settings/mcp/context7")
        .await
        .body;
    let used = &page[page.find("Used by").unwrap()..];
    assert!(used.contains("maestro"));
    assert!(!used.contains("/settings/projects/tool/mcp/context7"));
}

#[tokio::test]
async fn a_held_back_edit_counts_every_value_but_names_only_the_logins_own() {
    let dashboard = two_projects();
    dashboard
        .store
        .add_collaborator("maestro", MAESTRO_ONLY, "test")
        .unwrap();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-x");
    store_value_for(&dashboard, "tool", "Bearer sk-y");

    let reply = dashboard
        .post_as(
            MAESTRO_ONLY,
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=X-Api-Key",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Not saved yet."));
    assert!(reply.body.contains("Save and delete 2 values"));
    assert!(reply.body.contains("maestro: <code>Authorization</code>"));
    assert!(!reply.body.contains("tool: <code>Authorization</code>"));
    assert!(reply.body.contains("1 value in a project you cannot see"));
}

#[tokio::test]
async fn a_held_back_edit_hides_every_affected_project_from_a_non_collaborator() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-x");
    store_value_for(&dashboard, "tool", "Bearer sk-y");

    let reply = dashboard
        .post_as(
            STRANGER,
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=X-Api-Key",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Save and delete 2 values"));
    assert!(reply.body.contains("2 values in projects you cannot see"));
    assert!(!reply.body.contains("maestro: <code>Authorization</code>"));
    assert!(!reply.body.contains("tool: <code>Authorization</code>"));
}

#[tokio::test]
async fn an_edit_that_deletes_stored_values_waits_for_a_confirm() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-x");

    // Renaming the header would delete maestro's value, so nothing is saved.
    let reply = dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=X-Api-Key",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Not saved yet."));
    assert!(reply.body.contains("maestro: <code>Authorization</code>"));
    assert!(
        reply
            .body
            .contains(r#"<input type="hidden" name="confirm" value="on">"#)
    );
    assert!(reply.body.contains("Save and delete 1 value"));
    assert!(reply.body.contains(r#"<div class="save-bar dirty">"#));
    // The page keeps the posted name, ready to confirm.
    assert!(reply.body.contains(r#"value="X-Api-Key""#));
    assert_eq!(
        dashboard
            .store
            .mcp_server("context7")
            .unwrap()
            .unwrap()
            .as_remote()
            .unwrap()
            .1
            .to_vec(),
        vec!["Authorization".to_owned()]
    );
    assert!(has_value(&dashboard, "maestro", "Authorization"));

    // Confirming saves it and deletes the value.
    let reply = dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fmcp&header=X-Api-Key&confirm=on",
        )
        .await;
    assert!(reply.body.contains("Saved."));
    assert_eq!(
        dashboard
            .store
            .mcp_server("context7")
            .unwrap()
            .unwrap()
            .as_remote()
            .unwrap()
            .1
            .to_vec(),
        vec!["X-Api-Key".to_owned()]
    );
    assert!(!has_value(&dashboard, "maestro", "Authorization"));
}

#[tokio::test]
async fn an_origin_change_with_stored_values_waits_for_a_confirm() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-x");
    let reply = dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fother.example%2Fmcp&header=Authorization",
        )
        .await;
    assert!(reply.body.contains("Not saved yet."));
    assert_eq!(
        dashboard
            .store
            .mcp_server("context7")
            .unwrap()
            .unwrap()
            .as_remote()
            .unwrap()
            .0,
        "https://mcp.context7.com/mcp"
    );
    assert!(has_value(&dashboard, "maestro", "Authorization"));
}

#[tokio::test]
async fn an_edit_that_deletes_nothing_saves_at_once() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    // No project stored a value, so a rename deletes nothing.
    let reply = dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fv2&header=X-Api-Key",
        )
        .await;
    assert!(reply.body.contains("Saved."));
    assert!(!reply.body.contains("Not saved yet."));
}

#[tokio::test]
async fn deleting_a_server_needs_its_name_typed() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    store_value_for(&dashboard, "maestro", "Bearer sk-x");
    let page = dashboard.get("/settings/mcp/context7").await.body;
    assert!(page.contains("deletes the 1 stored value for it"));
    assert!(page.contains(r#"name="confirm_name""#));

    for form in ["", "confirm_name=context", "confirm_name=Context7"] {
        let reply = dashboard.post("/settings/mcp/context7/delete", form).await;
        assert_eq!(reply.status, StatusCode::OK, "{form}");
        assert!(reply.body.contains("Nothing was deleted."), "{form}");
        assert!(dashboard.store.mcp_server("context7").unwrap().is_some());
    }

    let reply = dashboard
        .post("/settings/mcp/context7/delete", "confirm_name=context7")
        .await;
    assert!(reply.body.contains("Deleted."));
    assert!(dashboard.store.mcp_server("context7").unwrap().is_none());
}

/// Whether the panel `key` on a project page is hidden.
fn panel_hidden(body: &str, key: &str) -> bool {
    let start = body
        .find(&format!(r#"data-panel="{key}""#))
        .unwrap_or_else(|| panic!("no panel {key}"));
    let tag_end = start + body[start..].find('>').unwrap();
    body[start..tag_end].contains("hidden")
}

/// The `<a>` tag that opens `part` on a project page, up to its `>`.
fn pipe_node<'a>(body: &'a str, part: &str) -> &'a str {
    let at = body
        .find(&format!(r#"data-part="{part}""#))
        .unwrap_or_else(|| panic!("no node {part}"));
    let start = body[..at].rfind("<a ").unwrap();
    let end = at + body[at..].find("</a>").unwrap();
    &body[start..end]
}

#[tokio::test]
async fn the_pipeline_runs_the_stages_in_order_with_the_gates_between() {
    let body = dashboard().get("/settings/projects/maestro").await.body;
    let order = [
        "clarify",
        "spec",
        "gate-spec",
        "plan",
        "gate-plan",
        "implement",
        "gate-publish",
        "review",
        "merge",
    ];
    let positions: Vec<usize> = order
        .iter()
        .map(|part| {
            body.find(&format!(
                r#"href="/settings/projects/maestro?stage={part}""#
            ))
            .unwrap_or_else(|| panic!("no link to {part}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{positions:?}"
    );
    assert!(body.contains(r#"classList.add("pipeline-js")"#));
}

#[tokio::test]
async fn the_stage_query_opens_one_panel_and_marks_its_node() {
    let dashboard = dashboard();
    let body = dashboard
        .get("/settings/projects/maestro?stage=gate-plan")
        .await
        .body;
    assert!(!panel_hidden(&body, "gate-plan"));
    for other in ["clarify", "review", "gate-spec", "merge"] {
        assert!(panel_hidden(&body, other), "{other}");
    }
    assert!(pipe_node(&body, "gate-plan").contains(r#"aria-current="step""#));
    assert_eq!(body.matches(r#"aria-current="step""#).count(), 1);
    assert!(body.contains(r#"action="/settings/projects/maestro?stage=gate-plan""#));

    // No value or an unknown one opens the first stage.
    for uri in [
        "/settings/projects/maestro",
        "/settings/projects/maestro?stage=nope",
        "/settings/projects/maestro?stage=gate-nope",
    ] {
        let body = dashboard.get(uri).await.body;
        assert!(!panel_hidden(&body, "clarify"), "{uri}");
        assert!(panel_hidden(&body, "implement"), "{uri}");
    }
}

#[tokio::test]
async fn each_node_shows_its_model_effort_and_servers() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=fable&spec=&plan=&implement=&review=opus\
             &clarify_effort=high&gates=on\
             &mcp.context7.pipeline=on&mcp.context7.stage.clarify=on",
        )
        .await;
    let body = dashboard.get("/settings/projects/maestro").await.body;

    let clarify = pipe_node(&body, "clarify");
    assert!(clarify.contains(">fable</span>"));
    assert_eq!(clarify.matches(r#"<i class="on">"#).count(), 3);
    assert!(clarify.contains(r#"data-server-label="context7.clarify">context7"#));

    let spec = pipe_node(&body, "spec");
    assert!(spec.contains(">CLI default</span>"));
    assert!(spec.contains(r#"data-server-label="context7.spec" hidden"#));

    // LLM review is off, so the review stage reads Off whatever its model.
    let review = pipe_node(&body, "review");
    assert!(review.contains("pipe-node off"));
    assert!(review.contains(">Off</span>"));

    let merge = pipe_node(&body, "merge");
    assert!(merge.contains("You, on GitHub"));
}

#[tokio::test]
async fn a_gate_node_shows_when_it_is_on_and_when_it_differs_from_the_default() {
    let dashboard = dashboard();
    // The defaults are spec and plan on. maestro turns plan off and publish on.
    dashboard
        .store
        .save_project_gates(
            "maestro",
            Gates {
                spec: true,
                plan: false,
                publish: true,
            },
        )
        .unwrap();
    let body = dashboard
        .get("/settings/projects/maestro?stage=gate-plan")
        .await
        .body;
    assert!(pipe_node(&body, "gate-spec").contains(r#"class="gate-node on""#));
    assert!(pipe_node(&body, "gate-plan").contains(r#"class="gate-node changed""#));
    assert!(pipe_node(&body, "gate-publish").contains(r#"class="gate-node on changed""#));

    let panel = &body[body.find(r#"data-panel="gate-plan""#).unwrap()..];
    let panel = &panel[..panel.find(r#"data-panel="gate-publish""#).unwrap()];
    assert!(panel.contains("Factory default: <strong>on</strong>"));
    assert!(panel.contains(r#"<span class="gate-changed">Changed for maestro. </span>"#));
    assert!(panel.contains(r#"data-gate-reset="plan""#));
}

#[tokio::test]
async fn a_save_comes_back_to_the_open_panel() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro?stage=merge",
            "clarify=&spec=&plan=&implement=&review=&gates=on&auto_merge=on",
        )
        .await;
    assert!(reply.body.contains("Saved."));
    assert!(!panel_hidden(&reply.body, "merge"));
    assert!(
        reply
            .body
            .contains(r#"action="/settings/projects/maestro?stage=merge""#)
    );
    assert!(pipe_node(&reply.body, "merge").contains("Automatic"));
}

#[tokio::test]
async fn a_pipeline_stage_switch_turns_a_server_on_in_that_stage() {
    let dashboard = dashboard();
    add_context7(&dashboard).await;
    let post = |stages: &'static str| {
        let dashboard = &dashboard;
        async move {
            dashboard
                .post(
                    "/settings/projects/maestro",
                    &format!(
                        "clarify=&spec=&plan=&implement=&review=&gates=on&mcp.context7.pipeline=on{stages}"
                    ),
                )
                .await
        }
    };
    let saved = || dashboard.store.project_mcp("maestro", "context7").unwrap();

    // A server that is off and stays off is not written.
    post("").await;
    assert!(!saved().enabled);

    // Switching a stage on turns the server on in that stage only.
    post("&mcp.context7.stage.plan=on&mcp.context7.stage.review=on").await;
    let settings = saved();
    assert!(settings.enabled);
    assert!(settings.stages.plan && settings.stages.review);
    assert!(!settings.stages.clarify && !settings.stages.spec && !settings.stages.implement);

    // Switching every stage off stops the server and keeps its stages.
    post("").await;
    let settings = saved();
    assert!(!settings.enabled);
    assert!(settings.stages.plan && settings.stages.review);
}

/// A [`Event::StartReview`] on a head no earlier pass read, so the reviewer
/// runs.
fn start_review() -> Event {
    static HEADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = HEADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Event::StartReview {
        head: format!("head{n}"),
    }
}

fn file_dashboard() -> (Dashboard, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    (dashboard_with(store), dir, path)
}

fn count(path: &Path, sql: &str, project: &str) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(sql, [project], |row| row.get(0))
        .unwrap()
}

fn fill_project(dashboard: &Dashboard, project: &str, job: i64) {
    let store = &dashboard.store;
    store
        .save_project_models(project, &StageModels::default())
        .unwrap();
    store.save_project_settings(project, true, true).unwrap();
    store
        .save_project_renovate_agent(
            project,
            Some(&factory_server::RenovateAgent {
                agent: "claude".into(),
                model: None,
                effort: "low".into(),
            }),
        )
        .unwrap();
    store
        .save_project_maintenance(
            project,
            &factory_server::Maintenance {
                renovate: true,
                renovate_author: None,
                packaging: None,
            },
        )
        .unwrap();
    store
        .put_secret(
            factory_server::SecretRef {
                project,
                server: "context7",
                header: "Authorization",
            },
            "local",
            b"sealed",
        )
        .unwrap();
    store
        .set_forge_account(
            ForgeTokenOwner::Project {
                project,
                forge: &ForgeKind::GitHub,
            },
            &ForgeAccount {
                login: "bot".into(),
                id: 7,
            },
        )
        .unwrap();
    store
        .put_secret(
            factory_server::SecretRef {
                project,
                server: "",
                header: "forge-project",
            },
            "local",
            b"sealed",
        )
        .unwrap();
    store.add_cost(job, 1.5, false).unwrap();
}

#[tokio::test]
async fn removing_a_project_cancels_its_jobs_and_deletes_its_data() {
    let (dashboard, _dir, path) = file_dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    fill_project(&dashboard, "maestro", id);

    let reply = dashboard
        .post("/settings/projects/maestro/remove", "confirm=maestro")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some("/settings?notice=Removed%20project%20maestro.")
    );

    let row = dashboard.store.get(id).unwrap();
    assert_eq!(row.job.stage(), &Stage::Cancelled);
    let log = dashboard.store.logs(id, 1).unwrap().pop().unwrap();
    assert_eq!(log.text, "Cancelled because the project was removed.");

    for (table, column) in [
        ("projects", "name"),
        ("project_models", "project"),
        ("project_settings", "project"),
        ("project_gates", "project"),
        ("project_efforts", "project"),
        ("project_agents", "project"),
        ("project_renovate_agent", "project"),
        ("project_mcp", "project"),
        ("project_mcp_tools", "project"),
        ("project_mcp_tool_refreshes", "project"),
        ("project_totals", "project"),
        ("project_collaborators", "project"),
        ("project_maintenance", "project"),
        ("project_forge_accounts", "project"),
        ("secrets", "project"),
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1");
        assert_eq!(count(&path, &sql, "maestro"), 0, "{table}");
    }
    let costs = "SELECT COUNT(*) FROM job_costs WHERE job_id IN
                 (SELECT id FROM jobs WHERE project = ?1)";
    assert_eq!(count(&path, costs, "maestro"), 0);
    let events = "SELECT COUNT(*) FROM job_events WHERE event = 'cost' AND job_id IN
                  (SELECT id FROM jobs WHERE project = ?1)";
    assert_eq!(count(&path, events, "maestro"), 0);
    assert_eq!(
        count(
            &path,
            "SELECT COUNT(*) FROM jobs WHERE project = ?1",
            "maestro"
        ),
        1
    );

    assert_eq!(
        dashboard.get("/?project=maestro").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dashboard.get(&format!("/jobs/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn removing_a_project_without_its_name_changes_nothing() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    for form in [
        "confirm=",
        "confirm=maestr",
        "confirm=other",
        "confirm=%20maestro%20",
        "",
    ] {
        let reply = dashboard
            .post("/settings/projects/maestro/remove", form)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{form}");
        assert!(reply.body.contains("Nothing was removed."), "{form}");
    }
    assert!(dashboard.store.project("maestro").unwrap().is_some());
    assert!(dashboard.store.is_collaborator("maestro", USER).unwrap());
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Requested
    );
}

#[tokio::test]
async fn a_non_collaborator_cannot_remove_a_project() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_as(
            STRANGER,
            "/settings/projects/maestro/remove",
            "confirm=maestro",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(dashboard.store.project("maestro").unwrap().is_some());
}

#[tokio::test]
async fn the_project_page_has_a_remove_project_form() {
    let page = dashboard().get("/settings/projects/maestro").await;
    assert!(
        page.body
            .contains("action=\"/settings/projects/maestro/remove\"")
    );
    assert!(page.body.contains("Remove maestro"));
}

#[tokio::test]
async fn the_removal_notice_shows_on_the_page_settings_lands_on() {
    let dashboard = dashboard();
    let reply = dashboard
        .get("/settings?notice=Removed%20project%20gone.")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let landing = reply.location.unwrap();
    let page = dashboard.get(&landing).await;
    assert!(page.body.contains("Removed project gone."));
    let page = dashboard
        .get_as(STRANGER, "/settings?notice=Removed%20project%20gone.")
        .await;
    let landing = page.location.unwrap();
    assert!(landing.starts_with("/settings/notifications?notice="));
    let page = dashboard.get_as(STRANGER, &landing).await;
    assert!(page.body.contains("Removed project gone."));
}

#[tokio::test]
async fn onboarding_a_removed_name_again_starts_with_an_empty_board() {
    let (dashboard, _dir, _path) = file_dashboard();
    seed_user_token(&dashboard.store, "bob@example.com");
    let id = dashboard.new_job().await;
    let job_dir = dashboard.data.path().join("jobs").join(id.to_string());
    fs::create_dir_all(&job_dir).unwrap();
    dashboard
        .post("/settings/projects/maestro/remove", "confirm=maestro")
        .await;
    assert!(dashboard.store.get(id).is_ok());

    let reply = dashboard
        .post_as(
            "bob@example.com",
            "/onboard",
            "repo=acme%2Fmaestro&name=maestro&action=skip",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(matches!(
        dashboard.store.get(id),
        Err(StoreError::NotFound(_))
    ));
    assert!(!job_dir.exists());

    let board = dashboard
        .get_as("bob@example.com", "/?project=maestro")
        .await;
    assert_eq!(board.status, StatusCode::OK);
    assert!(!board.body.contains("Add a reboot button</a>"));
    assert_eq!(
        collaborator_logins(&dashboard, "maestro"),
        vec!["bob@example.com".to_owned()]
    );
}

#[tokio::test]
async fn onboarding_is_refused_while_a_leftover_job_has_a_task() {
    let (dashboard, _dir, _path) = file_dashboard();
    seed_user_token(&dashboard.store, "bob@example.com");
    let id = dashboard.new_job().await;
    dashboard
        .post("/settings/projects/maestro/remove", "confirm=maestro")
        .await;
    dashboard
        .slots
        .register_task(id, false, |_| tokio::spawn(std::future::pending()));

    let reply = dashboard
        .post_as(
            "bob@example.com",
            "/onboard",
            "repo=acme%2Fmaestro&name=maestro&action=skip",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Try again in a minute."));
    assert!(dashboard.store.get(id).is_ok());
    assert!(dashboard.store.project("maestro").unwrap().is_none());

    dashboard.slots.begin_stop(id).unwrap();
    dashboard.slots.end_stop(id);
    let reply = dashboard
        .post_as(
            "bob@example.com",
            "/onboard",
            "repo=acme%2Fmaestro&name=maestro&action=skip",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

struct GatedChecker {
    calls: std::sync::atomic::AtomicUsize,
    gate: tokio::sync::Semaphore,
}

impl RepoChecker for GatedChecker {
    fn check<'a>(
        &'a self,
        _account: &'a Account,
        _repo: &'a str,
        _base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                self.gate.acquire().await.unwrap().forget();
            }
            Ok(RepoReport {
                errors: Vec::new(),
                warnings: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn concurrent_onboarding_of_one_name_cannot_delete_the_new_projects_jobs() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "acme/old",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let old = store.create("maestro", "alice", "Old request").unwrap();
    store.remove_project("maestro").unwrap();
    let checker = Arc::new(GatedChecker {
        calls: std::sync::atomic::AtomicUsize::new(0),
        gate: tokio::sync::Semaphore::new(0),
    });
    for login in ["bob@example.com", "carol@example.com"] {
        seed_user_token(&store, login);
    }
    let dashboard = Arc::new(dashboard_with_github(
        Arc::clone(&store),
        Arc::clone(&checker) as Arc<dyn RepoChecker>,
    ));
    let onboard = |login: &'static str| {
        let dashboard = Arc::clone(&dashboard);
        tokio::spawn(async move {
            dashboard
                .post_as(
                    login,
                    "/onboard",
                    "repo=acme%2Fnew&name=maestro&action=skip",
                )
                .await
        })
    };

    let first = onboard("bob@example.com");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let second = onboard("carol@example.com");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(checker.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    checker.gate.add_permits(1);
    let first = first.await.unwrap();
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    assert!(matches!(store.get(old), Err(StoreError::NotFound(_))));
    let new = store
        .create("maestro", "bob@example.com", "New request")
        .unwrap();
    let second = second.await.unwrap();
    assert_eq!(second.status, StatusCode::OK);
    assert!(second.body.contains("already exists"));
    assert!(store.get(new).is_ok());
}

const ADMIN_ROUTES: &[&str] = &[
    "/settings/workers",
    "/settings/gates",
    "/settings/polling",
    "/settings/renovate",
    "/settings/claude",
    "/settings/agents/claude",
    "/settings/agents/claude/token",
    "/settings/agents/codex",
    "/settings/agents/codex/auth",
    "/settings/agents/codex/login",
    "/settings/agents/codex/prices",
    "/settings/agents/copilot",
    "/settings/agents/copilot/token",
    "/settings/agents/copilot/login",
    "/settings/mcp",
    "/settings/mcp/anything",
    "/settings/mcp/anything/delete",
];

fn admin_dashboard(admins: &[&str]) -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let secrets = Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    dashboard_with_admins(
        store,
        Arc::new(FakeRepos::fit()),
        secrets,
        admins.iter().map(|login| (*login).to_owned()).collect(),
    )
}

#[tokio::test]
async fn a_login_outside_the_admin_list_gets_403_on_every_factory_settings_route() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    for uri in ADMIN_ROUTES {
        let reply = dashboard.get_as(USER, uri).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "GET {uri}");
        assert!(reply.body.contains("This page is for factory admins."));
        let reply = dashboard.post_as(USER, uri, "name=x").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "POST {uri}");
    }
}

#[tokio::test]
async fn an_admin_opens_the_factory_settings_pages_and_sees_their_groups() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    for uri in [
        "/settings/workers",
        "/settings/gates",
        "/settings/polling",
        "/settings/renovate",
        "/settings/agents/codex",
        "/settings/mcp",
    ] {
        let reply = dashboard.get_as("boss@example.com", uri).await;
        assert_eq!(reply.status, StatusCode::OK, "GET {uri}");
        assert!(reply.body.contains("Integrations"), "{uri}");
        assert!(reply.body.contains("href=\"/settings/renovate\""), "{uri}");
    }
    let body = dashboard
        .get_as("boss@example.com", "/settings/renovate")
        .await
        .body;
    assert!(body.contains("name=\"auto_reruns\""));
    assert!(body.contains("value=\"3\""));
    assert!(body.contains("value=\"7\""));
    let other = dashboard
        .get_as(USER, "/settings/projects/maestro")
        .await
        .body;
    assert!(!other.contains("/settings/renovate"));
}

#[tokio::test]
async fn an_admin_saves_the_renovate_limits() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    let reply = dashboard
        .post_as(
            "boss@example.com",
            "/settings/renovate",
            "auto_reruns=0&total_reruns=2",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    let settings = dashboard.store.settings().unwrap();
    assert_eq!(settings.renovate_auto_reruns, 0);
    assert_eq!(settings.renovate_total_reruns, 2);
}

#[tokio::test]
async fn the_renovate_page_refuses_bad_limits_and_stores_nothing() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    let before = dashboard.store.settings().unwrap();
    for form in [
        "auto_reruns=0&total_reruns=0",
        "auto_reruns=4&total_reruns=3",
        "auto_reruns=x&total_reruns=3",
        "auto_reruns=2.5&total_reruns=7",
        "auto_reruns=1&total_reruns=2.5",
        "auto_reruns=-1&total_reruns=3",
        "auto_reruns=&total_reruns=",
    ] {
        let reply = dashboard
            .post_as("boss@example.com", "/settings/renovate", form)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{form}");
        assert!(reply.body.contains("card failed"), "{form}");
        assert!(!reply.body.contains("Saved."), "{form}");
        assert_eq!(dashboard.store.settings().unwrap(), before, "{form}");
    }
}

#[tokio::test]
async fn the_sidebar_hides_the_factory_groups_from_a_non_admin() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    let body = dashboard
        .get_as(USER, "/settings/projects/maestro")
        .await
        .body;
    for hidden in [
        "Integrations",
        "Default gates",
        ">Agents<",
        "/settings/workers",
    ] {
        assert!(!body.contains(hidden), "{hidden}");
    }
    assert!(body.contains("maestro"));
}

#[tokio::test]
async fn the_pipeline_panel_links_the_server_registry_only_for_an_admin() {
    let dashboard = admin_dashboard(&["boss@example.com"]);
    dashboard
        .store
        .add_collaborator("maestro", "boss@example.com", "test")
        .unwrap();
    let uri = "/settings/projects/maestro";
    let admin = dashboard.get_as("boss@example.com", uri).await.body;
    let other = dashboard.get_as(USER, uri).await.body;
    assert!(admin.contains("href=\"/settings/mcp\""));
    assert!(!other.contains("href=\"/settings/mcp"));
}

#[tokio::test]
async fn the_admin_list_gets_exactly_the_logins_it_names() {
    let dashboard = admin_dashboard(&["a@x", "b@y"]);
    for (login, status) in [
        ("a@x", StatusCode::OK),
        ("b@y", StatusCode::OK),
        ("c@z", StatusCode::FORBIDDEN),
        ("local", StatusCode::FORBIDDEN),
    ] {
        let reply = dashboard.get_as(login, "/settings/workers").await;
        assert_eq!(reply.status, status, "{login}");
    }
}

#[test]
fn factory_admins_parse_trims_drops_empties_and_defaults() {
    use factory_server::parse_admins;
    assert_eq!(parse_admins(None), ["talhahavadar@hotmail.com"]);
    assert_eq!(parse_admins(Some(" a@x , ,b@y,")), ["a@x", "b@y"]);
    assert_eq!(parse_admins(Some("")), Vec::<String>::new());
}

async fn notifications_as(dashboard: &Dashboard, login: &str) -> serde_json::Value {
    let reply = dashboard.get_as(login, "/notifications").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type.as_deref(), Some("application/json"));
    serde_json::from_str(&reply.body).unwrap()
}

fn to_plan_approval(dashboard: &Dashboard, id: i64) {
    to_spec_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![PlannedPr {
            title: "maestrod: add a reboot endpoint".into(),
            summary: "POST /reboot".into(),
            number: None,
            description: None,
            notes: None,
        }])),
    );
}

fn to_review(dashboard: &Dashboard, id: i64) {
    to_plan_approval(dashboard, id);
    dashboard.apply(id, Event::Approve);
    dashboard.apply(id, Event::Agent(AgentResult::CommitsReady(ready())));
    dashboard.apply(id, Event::ChecksPassed);
    dashboard.apply(id, Event::Published { number: 15 });
}

#[tokio::test]
async fn a_factory_job_notifies_every_collaborator_with_notifications_on() {
    let dashboard = dashboard();
    let store = &dashboard.store;
    store
        .add_collaborator("maestro", "bob@example.com", "test")
        .unwrap();
    store
        .add_collaborator("maestro", "carol@example.com", "test")
        .unwrap();
    store.set_notifications("carol@example.com", false).unwrap();
    let id = store
        .create("maestro", FACTORY_REQUESTER, "Bump a crate")
        .unwrap();
    to_spec_approval(&dashboard, id);

    for login in [USER, "bob@example.com"] {
        let items = notifications_as(&dashboard, login).await["items"].clone();
        assert_eq!(items[0]["job"], id, "{login}");
        assert_eq!(items[0]["kind"], "needs-you", "{login}");
    }
    for login in ["carol@example.com", "mallory@example.com"] {
        let items = notifications_as(&dashboard, login).await["items"].clone();
        assert_eq!(items, json!([]), "{login}");
    }
}

#[tokio::test]
async fn notifications_list_a_job_at_the_spec_gate_and_a_new_key_at_the_plan_gate() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    assert_eq!(notifications_as(&dashboard, USER).await["items"], json!([]));

    to_spec_approval(&dashboard, id);
    let first = notifications_as(&dashboard, USER).await;
    assert_eq!(first["enabled"], true);
    let item = &first["items"][0];
    assert_eq!(item["job"], id);
    assert_eq!(item["kind"], "needs-you");
    assert_eq!(item["url"], format!("/jobs/{id}"));
    assert!(item["key"].as_str().unwrap().starts_with(&format!("{id}:")));
    assert_ne!(item["title"].as_str().unwrap(), "");

    assert_eq!(
        notifications_as(&dashboard, USER).await["items"],
        first["items"]
    );

    dashboard.apply(id, Event::Approve);
    dashboard.apply(
        id,
        Event::Agent(AgentResult::Plan(vec![PlannedPr {
            title: "t".into(),
            summary: "s".into(),
            number: None,
            description: None,
            notes: None,
        }])),
    );
    let second = notifications_as(&dashboard, USER).await;
    assert_eq!(second["items"][0]["kind"], "needs-you");
    assert_ne!(second["items"][0]["key"], item["key"]);
}

fn observe_renovate(dashboard: &Dashboard, id: i64, head: &str, ci: factory_core::RenovateCi) {
    dashboard.apply(
        id,
        Event::RenovateObserved {
            head: head.into(),
            ci,
            limits: factory_core::RetryLimits { auto: 0, total: 7 },
            auto_merge: false,
            access: Box::default(),
        },
    );
}

fn passed_with_details() -> factory_core::RenovateCi {
    factory_core::RenovateCi::Passed {
        details: Some(Box::new(factory_core::PrDiff {
            diff: "+serde = 1.0.2".into(),
            body: "Bumps serde".into(),
        })),
    }
}

fn renovate_to_ready(dashboard: &Dashboard, id: i64, head: &str) {
    observe_renovate(dashboard, id, head, passed_with_details());
    dashboard.apply(
        id,
        Event::Agent(AgentResult::DiffVerdict(Box::new(
            factory_core::DiffVerdict {
                safe: true,
                reason: "only a version bump".into(),
                summary: "Bumps serde".into(),
                from: None,
                to: None,
            },
        ))),
    );
    observe_renovate(
        dashboard,
        id,
        head,
        factory_core::RenovateCi::Passed { details: None },
    );
    assert!(matches!(
        dashboard.turn(id),
        Turn::User(Ask::Renovate {
            state: factory_core::RenovateWait::Ready,
            ..
        })
    ));
}

#[tokio::test]
async fn a_ready_renovate_job_keeps_its_notification_key_between_watches() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_renovate_job("maestro", 7, "Update serde", "https://example/pull/7")
        .unwrap()
        .unwrap();
    renovate_to_ready(&dashboard, id, "h1");
    let events = dashboard.store.events(id).unwrap().len();
    let first = notifications_as(&dashboard, USER).await["items"].clone();
    assert_eq!(first.as_array().unwrap().len(), 1);
    assert_eq!(first[0]["kind"], "needs-you");

    for _ in 0..3 {
        observe_renovate(
            &dashboard,
            id,
            "h1",
            factory_core::RenovateCi::Passed { details: None },
        );
    }
    assert_eq!(dashboard.store.events(id).unwrap().len(), events);
    assert_eq!(notifications_as(&dashboard, USER).await["items"], first);

    renovate_to_ready(&dashboard, id, "h2");
    let second = notifications_as(&dashboard, USER).await["items"].clone();
    assert_eq!(second.as_array().unwrap().len(), 1);
    assert_eq!(second[0]["kind"], "needs-you");
    assert_ne!(second[0]["key"], first[0]["key"]);
}

#[tokio::test]
async fn repeated_renovate_watches_of_pending_or_missing_checks_record_nothing() {
    let dashboard = dashboard();
    for (n, ci) in [
        factory_core::RenovateCi::Pending,
        factory_core::RenovateCi::NoChecks,
    ]
    .into_iter()
    .enumerate()
    {
        let id = dashboard
            .store
            .create_renovate_job(
                "maestro",
                10 + n as u64,
                "Update serde",
                "https://example/pull/7",
            )
            .unwrap()
            .unwrap();
        observe_renovate(&dashboard, id, "h1", ci.clone());
        let events = dashboard.store.events(id).unwrap().len();
        for _ in 0..3 {
            observe_renovate(&dashboard, id, "h1", ci.clone());
        }
        assert_eq!(dashboard.store.events(id).unwrap().len(), events);
    }
}

#[tokio::test]
async fn notifications_go_only_to_the_requester() {
    let dashboard = dashboard();
    dashboard
        .store
        .add_collaborator("maestro", "bob@example.com", "test")
        .unwrap();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    let bob = notifications_as(&dashboard, "bob@example.com").await;
    assert_eq!(bob["enabled"], true);
    assert_eq!(bob["items"], json!([]));
    assert_eq!(
        notifications_as(&dashboard, USER).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn notifications_skip_a_project_the_requester_left() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard
        .store
        .add_collaborator("maestro", "bob@example.com", "test")
        .unwrap();
    dashboard
        .store
        .remove_collaborator("maestro", USER)
        .unwrap();
    assert_eq!(notifications_as(&dashboard, USER).await["items"], json!([]));
}

#[tokio::test]
async fn a_failed_job_notifies_until_it_is_dismissed() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
    dashboard.apply(
        id,
        Event::AgentFailed {
            reason: "boom".into(),
        },
    );
    let data = notifications_as(&dashboard, USER).await;
    assert_eq!(data["items"][0]["kind"], "failed");

    dashboard.apply(id, Event::Dismiss);
    assert_eq!(notifications_as(&dashboard, USER).await["items"], json!([]));
}

#[tokio::test]
async fn a_pull_request_in_review_notifies_only_with_auto_merge_off() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_review(&dashboard, id);
    let data = notifications_as(&dashboard, USER).await;
    assert_eq!(data["items"][0]["kind"], "needs-you");

    dashboard
        .store
        .save_project_auto_merge("maestro", true)
        .unwrap();
    assert_eq!(notifications_as(&dashboard, USER).await["items"], json!([]));
}

#[tokio::test]
async fn notifications_are_empty_when_the_login_turned_them_off() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    to_spec_approval(&dashboard, id);
    dashboard.store.set_notifications(USER, false).unwrap();
    assert_eq!(
        notifications_as(&dashboard, USER).await,
        json!({"enabled": false, "items": []})
    );
    dashboard.store.set_notifications(USER, true).unwrap();
    assert_eq!(
        notifications_as(&dashboard, USER).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn the_board_asks_for_permission_only_while_notifications_are_on() {
    let dashboard = dashboard();
    let page = dashboard.get("/").await;
    assert!(
        page.body
            .contains("Get a browser notification when a job needs you")
    );
    assert!(page.body.contains("href=\"/settings/notifications\""));
    assert!(page.body.contains("Notification.permission"));

    dashboard.store.set_notifications(USER, false).unwrap();
    let page = dashboard.get("/").await;
    assert!(
        !page
            .body
            .contains("Get a browser notification when a job needs you")
    );
}

#[test]
fn a_job_that_changes_between_the_two_reads_keeps_its_state_and_event_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    let other = Store::open(&path).unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("a"),
        )
        .unwrap();
    let id = store.create("maestro", "a", "Add a button").unwrap();
    store.apply(id, Event::Start).unwrap();
    store
        .apply(id, Event::Agent(AgentResult::Clarified))
        .unwrap();
    store
        .apply(id, Event::Agent(AgentResult::Spec("spec".into())))
        .unwrap();
    let before = store.list_with_latest_event().unwrap().jobs;
    let (job, event) = (before[0].0.clone(), before[0].1);

    let during = store
        .list_with_latest_event_between(|| {
            other.apply(id, Event::Approve).unwrap();
        })
        .unwrap()
        .jobs;
    assert_eq!(during[0], (job, event));

    let after = store.list_with_latest_event().unwrap().jobs;
    assert!(after[0].1 > event);
    assert_ne!(after[0].0.job.stage(), during[0].0.job.stage());
}

#[test]
fn a_reopened_database_orders_its_reads_after_the_earlier_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let mut last = Store::open(&path)
        .unwrap()
        .list_with_latest_event()
        .unwrap()
        .order;
    for _ in 0..3 {
        let order = Store::open(&path)
            .unwrap()
            .list_with_latest_event()
            .unwrap()
            .order;
        assert_eq!(order.1, 1);
        assert!(order > last);
        last = order;
    }
}

#[test]
fn each_read_of_the_jobs_orders_after_the_one_before() {
    let store = Store::open_in_memory().unwrap();
    let first = store.list_with_latest_event().unwrap().order;
    let second = store.list_with_latest_event().unwrap().order;
    assert!(second > first);
}

#[tokio::test]
async fn each_notifications_response_carries_a_greater_snapshot_than_the_last() {
    let dashboard = dashboard();
    let first = notifications_as(&dashboard, USER).await["snapshot"].clone();
    let second = notifications_as(&dashboard, USER).await["snapshot"].clone();
    let pair = |value: &serde_json::Value| (value[0].as_u64().unwrap(), value[1].as_u64().unwrap());
    assert!(pair(&second) > pair(&first));
}

const PACKAGING_FIELDS: [&str; 4] = [
    "name=\"packaging_type\"",
    "name=\"manifest_files\"",
    "name=\"build_config_files\"",
    "name=\"upstream_repo\"",
];

const PACKAGING_FORM: &str = "renovate=on&packaging=on&packaging_type=deb\
    &manifest_files=%20debian/control%20%0D%0A%0D%0Asnap/snapcraft.yaml\
    &build_config_files=&upstream_repo=o%2Ftool";

/// Seals `token` as the project token of `maestro` for `login`.
fn seed_project_token(dashboard: &Dashboard, token: &str, login: &str, id: u64) {
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .put_forge_token(
            ForgeTokenOwner::Project {
                project: "maestro",
                forge: &ForgeKind::GitHub,
            },
            token,
            &ForgeAccount {
                login: login.into(),
                id,
            },
        )
        .unwrap();
}

/// A dashboard with the fake `gh` and the Write token `tok-alice` stored as
/// the project token, which Renovate needs.
fn dashboard_with_project_token() -> Dashboard {
    let dashboard = forge_token_dashboard(true);
    seed_project_token(&dashboard, "tok-alice", "alice-bot", 11);
    dashboard
}

#[tokio::test]
async fn a_project_starts_with_both_maintenance_switches_off_and_no_packaging_fields() {
    let dashboard = dashboard();
    assert_eq!(
        dashboard.store.project_maintenance("maestro").unwrap(),
        factory_server::Maintenance::default()
    );
    let page = dashboard.get("/settings/projects/maestro").await;
    assert!(page.body.contains("name=\"renovate\""));
    assert!(page.body.contains("name=\"packaging\""));
    for field in PACKAGING_FIELDS {
        assert!(!page.body.contains(field), "{field}");
    }
}

#[tokio::test]
async fn a_project_made_before_the_table_existed_reads_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Store::open(&path).unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    drop(store);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DROP TABLE project_maintenance", [])
        .unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.project_maintenance("maestro").unwrap(),
        factory_server::Maintenance::default()
    );
}

#[tokio::test]
async fn ticking_packaging_without_a_manifest_redraws_the_fields_and_stores_nothing() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            "/settings/projects/maestro/maintenance",
            "renovate=on&packaging=on",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Give at least one manifest file."));
    for field in PACKAGING_FIELDS {
        assert!(reply.body.contains(field), "{field}");
    }
    assert!(
        reply
            .body
            .contains("name=\"packaging\" value=\"on\" checked")
    );
    assert_eq!(
        dashboard.store.project_maintenance("maestro").unwrap(),
        factory_server::Maintenance::default()
    );
}

#[tokio::test]
async fn a_packaging_save_saves_and_renders_the_stored_values() {
    let dashboard = dashboard_with_project_token();
    let reply = dashboard
        .post("/settings/projects/maestro/maintenance", PACKAGING_FORM)
        .await;
    assert!(reply.body.contains("Saved."));
    let stored = dashboard.store.project_maintenance("maestro").unwrap();
    assert!(stored.renovate);
    let packaging = stored.packaging.unwrap();
    assert_eq!(packaging.kind, factory_server::PackagingType::Deb);
    assert_eq!(
        packaging.manifest_files,
        ["debian/control", "snap/snapcraft.yaml"]
    );
    assert_eq!(packaging.build_config_files, Vec::<String>::new());
    assert_eq!(packaging.upstream_repo, "o/tool");

    let page = dashboard.get("/settings/projects/maestro").await;
    for field in PACKAGING_FIELDS {
        assert!(page.body.contains(field), "{field}");
    }
    assert!(page.body.contains("debian/control\nsnap/snapcraft.yaml"));
    assert!(page.body.contains("value=\"o/tool\""));
    assert!(page.body.contains("<option value=\"deb\" selected"));
}

#[tokio::test]
async fn each_refused_maintenance_save_leaves_the_stored_row_unchanged() {
    let dashboard = dashboard_with_project_token();
    dashboard
        .post("/settings/projects/maestro/maintenance", PACKAGING_FORM)
        .await;
    let before = dashboard.store.project_maintenance("maestro").unwrap();
    for (form, message) in [
        (
            "packaging=on&manifest_files=a&upstream_repo=o%2Ftool",
            "needs Renovate pull requests on",
        ),
        (
            "renovate=on&packaging=on&manifest_files=%0D%0A&upstream_repo=o%2Ftool",
            "at least one manifest file",
        ),
        (
            "renovate=on&packaging=on&manifest_files=a&upstream_repo=tool",
            "owner/name",
        ),
        (
            "renovate=on&packaging=on&manifest_files=a&upstream_repo=a%2Fb%2Fc",
            "owner/name",
        ),
        (
            "renovate=on&packaging=on&packaging_type=rpm&manifest_files=a&upstream_repo=o%2Ft",
            "snap, deb or other",
        ),
    ] {
        let reply = dashboard
            .post("/settings/projects/maestro/maintenance", form)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{form}");
        assert!(reply.body.contains(message), "{form}");
        assert_eq!(
            dashboard.store.project_maintenance("maestro").unwrap(),
            before,
            "{form}"
        );
    }
}

#[tokio::test]
async fn turning_packaging_off_hides_its_fields_again() {
    let dashboard = dashboard_with_project_token();
    dashboard
        .post("/settings/projects/maestro/maintenance", PACKAGING_FORM)
        .await;
    let reply = dashboard
        .post("/settings/projects/maestro/maintenance", "renovate=on")
        .await;
    assert!(reply.body.contains("Saved."));
    for field in PACKAGING_FIELDS {
        assert!(!reply.body.contains(field), "{field}");
    }
    let stored = dashboard.store.project_maintenance("maestro").unwrap();
    assert!(stored.renovate && stored.packaging.is_none());
}

#[tokio::test]
async fn renovate_on_needs_a_project_token() {
    let dashboard = forge_token_dashboard(true);
    let reply = dashboard
        .post("/settings/projects/maestro/maintenance", "renovate=on")
        .await;
    assert!(reply.body.contains("need a project token"));
    assert!(
        reply
            .body
            .contains("/settings/projects/maestro#forge-account")
    );
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

#[tokio::test]
async fn renovate_on_reads_the_project_accounts_access() {
    let dashboard = forge_token_dashboard(true);
    let maintain = || async {
        dashboard
            .post("/settings/projects/maestro/maintenance", "renovate=on")
            .await
    };
    let renovate = || {
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    };

    project_post(&dashboard, USER, "forge-token", "token=tok-ghost").await;
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );

    project_post(&dashboard, USER, "forge-token", "token=tok-reader").await;
    let read = maintain().await;
    assert!(read.body.contains("Saved."));
    assert!(read.body.contains("reader-bot has only Read access"));
    assert!(read.body.contains("cannot rerun failed CI"));
    assert!(read.body.contains("cannot merge Renovate pull requests"));
    assert!(renovate());

    project_post(&dashboard, USER, "forge-token", "token=tok-alice").await;
    let write = maintain().await;
    assert!(write.body.contains("Saved."));
    assert!(!write.body.contains("Read access"));
    assert!(!write.body.contains("was not checked"));

    project_post(&dashboard, USER, "forge-token", "token=tok-admin").await;
    let admin = maintain().await;
    assert!(admin.body.contains("Saved."));
    assert!(!admin.body.contains("Read access"));

    project_post(&dashboard, USER, "forge-token", "token=tok-blind").await;
    let blind = maintain().await;
    assert!(blind.body.contains("Saved."));
    assert!(
        blind
            .body
            .contains("access of blind-bot to Badger-Embedded/maestro was not checked")
    );
}

#[tokio::test]
async fn a_project_account_that_lost_sight_of_the_repository_refuses_renovate() {
    let dashboard = forge_token_dashboard(true);
    seed_project_token(&dashboard, "tok-ghost", "ghost-bot", 55);
    let reply = dashboard
        .post("/settings/projects/maestro/maintenance", "renovate=on")
        .await;
    assert!(
        reply
            .body
            .contains("ghost-bot cannot see Badger-Embedded/maestro")
    );
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

#[tokio::test]
async fn account_metadata_without_a_sealed_token_does_not_turn_renovate_on() {
    let dashboard = forge_token_dashboard(true);
    dashboard
        .store
        .set_forge_account(
            project_owner(),
            &ForgeAccount {
                login: "bot".into(),
                id: 7,
            },
        )
        .unwrap();
    let reply = dashboard
        .post("/settings/projects/maestro/maintenance", "renovate=on")
        .await;
    assert!(reply.body.contains("need a project token"));
    assert!(!reply.body.contains("was not checked"));
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

/// Posts the Maintenance card as `login` with Renovate on, and runs
/// `meanwhile` while the access check of the `tok-slow` project token waits.
async fn maintain_while(dashboard: &Dashboard, login: &str, meanwhile: impl FnOnce()) -> Reply {
    let started = dashboard.data.path().join("started");
    let go = dashboard.data.path().join("go");
    let save = dashboard.post_as(
        login,
        "/settings/projects/maestro/maintenance",
        "renovate=on",
    );
    let driver = async {
        for _ in 0..500 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            started.exists(),
            "the access check did not start within 5 s, so the Renovate save never reached the forge"
        );
        meanwhile();
        std::fs::write(&go, "").unwrap();
    };
    tokio::join!(save, driver).0
}

fn slow_project_dashboard() -> Dashboard {
    let dashboard = forge_token_dashboard(true);
    seed_project_token(&dashboard, "tok-slow", "slow-bot", 77);
    dashboard
}

#[tokio::test]
async fn a_renovate_save_that_finishes_after_the_project_was_recreated_stores_nothing() {
    let dashboard = slow_project_dashboard();
    let reply = maintain_while(&dashboard, USER, || {
        dashboard.store.remove_project("maestro").unwrap();
        dashboard
            .store
            .create_project(
                "maestro",
                &ForgeKind::GitHub,
                "Badger-Embedded/maestro",
                "main",
                None,
                Some(USER),
            )
            .unwrap();
        dashboard.store.project_with_instance("maestro").unwrap();
    })
    .await;
    assert!(reply.body.contains("Nothing was saved"));
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

#[tokio::test]
async fn a_renovate_save_by_a_login_removed_meanwhile_stores_nothing() {
    let dashboard = slow_project_dashboard();
    dashboard
        .store
        .add_collaborator("maestro", DAVE, "test")
        .unwrap();
    let reply = maintain_while(&dashboard, DAVE, || {
        dashboard
            .store
            .remove_collaborator("maestro", DAVE)
            .unwrap();
    })
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

#[tokio::test]
async fn a_renovate_save_after_the_project_token_was_cleared_or_replaced_stores_nothing() {
    let dashboard = slow_project_dashboard();
    let reply = maintain_while(&dashboard, USER, || {
        LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
            .delete_forge_token(project_owner())
            .unwrap();
    })
    .await;
    assert!(reply.body.contains("A token changed"));
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );

    let dashboard = slow_project_dashboard();
    let reply = maintain_while(&dashboard, USER, || {
        seed_project_token(&dashboard, "tok-bob", "bob-bot", 22);
    })
    .await;
    assert!(reply.body.contains("A token changed"));
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

#[tokio::test]
async fn a_renovate_save_after_the_token_was_replaced_by_one_of_the_same_account_stores_nothing() {
    let dashboard = slow_project_dashboard();
    let reply = maintain_while(&dashboard, USER, || {
        seed_project_token(&dashboard, "tok-reader", "slow-bot", 77);
    })
    .await;
    assert!(reply.body.contains("A token changed"));
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

#[tokio::test]
async fn turning_renovate_off_needs_no_forge_and_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO projects (name, forge, repo, base) VALUES ('odd', 'elsewhere', 'o/odd', 'main')",
            [],
        )
        .unwrap();
    let dashboard = dashboard_with(store);
    let maintenance = factory_server::Maintenance {
        renovate: true,
        renovate_author: None,
        packaging: None,
    };
    dashboard
        .store
        .save_project_maintenance("odd", &maintenance)
        .unwrap();
    let reply = dashboard
        .post("/settings/projects/odd/maintenance", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    assert!(!dashboard.store.project_maintenance("odd").unwrap().renovate);
}

#[tokio::test]
async fn a_non_collaborator_cannot_save_maintenance_settings() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_as(
            STRANGER,
            "/settings/projects/maestro/maintenance",
            "renovate=on",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(
        !dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .renovate
    );
}

#[test]
fn maintenance_settings_round_trip_through_the_store() {
    let store = Store::open_in_memory().unwrap();
    let packaging = factory_server::Packaging {
        kind: factory_server::PackagingType::Other,
        manifest_files: vec!["a.yaml".into(), "b dir/c.yaml".into()],
        build_config_files: vec!["CMakeLists.txt".into()],
        upstream_repo: "o/tool".into(),
        upstream_forge: ForgeKind::GitHub,
    };
    let maintenance = factory_server::Maintenance {
        renovate: true,
        renovate_author: Some("renovate-bot".into()),
        packaging: Some(packaging),
    };
    assert!(maintenance.check().is_ok());
    store.save_project_maintenance("p", &maintenance).unwrap();
    assert_eq!(store.project_maintenance("p").unwrap(), maintenance);
    let off = factory_server::Maintenance::default();
    store.save_project_maintenance("p", &off).unwrap();
    assert_eq!(store.project_maintenance("p").unwrap(), off);
    assert_eq!(store.project_maintenance("other").unwrap(), off);
}

#[tokio::test]
async fn a_proposal_shows_a_card_and_row_actions_in_needs_you() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();

    let body = dashboard.get("/").await.body;
    assert_eq!(lanes_of(&body, id), ["inbox-you"]);
    let lane = lane_html(&body, "inbox-you");
    assert!(lane.contains(">proposal</span>"));
    assert!(lane.contains(&format!("action=\"/jobs/{id}/approve\"")));
    assert!(lane.contains("name=\"project\" value=\"maestro\""));

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains("Add the new build flag"));
    assert!(page.contains(&format!("action=\"/jobs/{id}/approve\"")));
    assert!(page.contains(&format!("action=\"/jobs/{id}/dismiss\"")));
}

#[tokio::test]
async fn approving_a_proposal_makes_the_approver_the_requester() {
    let dashboard = dashboard();
    dashboard
        .store
        .add_collaborator("maestro", "bob@example.com", "test")
        .unwrap();
    let id = dashboard
        .store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();
    assert_eq!(
        dashboard.store.get(id).unwrap().requested_by,
        FACTORY_REQUESTER
    );

    let reply = dashboard
        .post_as("bob@example.com", &format!("/jobs/{id}/approve"), "")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let row = dashboard.store.get(id).unwrap();
    assert_eq!(row.requested_by, "bob@example.com");
    assert_eq!(row.job.stage(), &Stage::Requested);
}

#[tokio::test]
async fn dismissing_a_proposal_moves_it_to_done_with_a_dismissed_badge() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();

    let reply = dashboard.post(&format!("/jobs/{id}/dismiss"), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.store.get(id).unwrap().requested_by,
        FACTORY_REQUESTER
    );

    let body = dashboard.get("/").await.body;
    assert_eq!(lanes_of(&body, id), ["inbox-done"]);
    assert!(lane_html(&body, "inbox-done").contains(">dismissed</span>"));
    assert!(!body.contains("inbox-you"));
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains(">dismissed</span>"));
    assert!(!page.contains(&format!("action=\"/jobs/{id}/approve\"")));
}

#[tokio::test]
async fn a_non_collaborator_cannot_approve_or_dismiss_a_proposal() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();

    for route in ["approve", "dismiss"] {
        let reply = dashboard
            .post_as(STRANGER, &format!("/jobs/{id}/{route}"), "")
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{route}");
    }
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Proposed
    );
}

#[tokio::test]
async fn a_proposal_notifies_every_collaborator_and_after_approval_only_the_approver() {
    let dashboard = dashboard();
    let store = &dashboard.store;
    store
        .add_collaborator("maestro", "bob@example.com", "test")
        .unwrap();
    let id = store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();

    for login in [USER, "bob@example.com"] {
        let items = notifications_as(&dashboard, login).await["items"].clone();
        assert_eq!(items[0]["job"], id, "{login}");
        assert_eq!(items[0]["kind"], "needs-you", "{login}");
    }
    assert_eq!(
        notifications_as(&dashboard, "mallory@example.com").await["items"],
        json!([])
    );

    store.approve_proposal(id, "bob@example.com").unwrap();
    dashboard.apply(id, Event::Start);
    dashboard.apply(id, Event::Agent(AgentResult::Clarified));
    dashboard.apply(id, Event::Agent(AgentResult::Spec("spec".into())));
    let bob = notifications_as(&dashboard, "bob@example.com").await["items"].clone();
    assert_eq!(bob[0]["job"], id);
    assert_eq!(notifications_as(&dashboard, USER).await["items"], json!([]));
}

#[tokio::test]
async fn a_dismissed_gap_stays_known_after_its_job_expires() {
    let dashboard = dashboard();
    let store = &dashboard.store;
    let kept = store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();
    let live = store
        .create_gap_job("maestro", 1, 43, "Drop the old flag")
        .unwrap();
    dashboard.apply(kept, Event::Dismiss);
    store.delete_job(kept, Path::new("/nonexistent")).unwrap();

    let gaps = store.gap_jobs("maestro").unwrap();
    let ids: Vec<(i64, bool)> = gaps.iter().map(|g| (g.id, g.dismissed)).collect();
    assert_eq!(ids, [(live, false), (kept, true)]);
    assert_eq!(gaps[1].request, "Add the new build flag");
    assert_eq!(gaps[1].source_pr, 42);
    assert_eq!(store.gap_jobs("other").unwrap().len(), 0);
}

#[tokio::test]
async fn a_stale_proposal_approval_cannot_pass_a_later_gate_or_change_the_requester() {
    let dashboard = dashboard();
    let store = &dashboard.store;
    store
        .save_project_gates(
            "maestro",
            Gates {
                spec: true,
                plan: true,
                publish: false,
            },
        )
        .unwrap();
    let id = store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();
    store.approve_proposal(id, "bob@example.com").unwrap();
    dashboard.apply(id, Event::Start);
    dashboard.apply(id, Event::Agent(AgentResult::Clarified));
    dashboard.apply(id, Event::Agent(AgentResult::Spec("spec".into())));

    assert!(store.approve_proposal(id, "carol@example.com").is_err());
    let row = store.get(id).unwrap();
    assert_eq!(row.requested_by, "bob@example.com");
    assert_eq!(row.job.stage(), &Stage::Spec);
    assert_eq!(row.job.turn(), &Turn::User(Ask::SpecApproval));
}

#[tokio::test]
async fn a_reused_job_id_does_not_hide_or_replace_an_earlier_dismissal() {
    let dashboard = dashboard();
    let store = &dashboard.store;
    let first = store
        .create_gap_job("maestro", 1, 42, "Add the new build flag")
        .unwrap();
    dashboard.apply(first, Event::Dismiss);
    store.delete_job(first, Path::new("/nonexistent")).unwrap();

    let reused = store
        .create_gap_job("maestro", 1, 43, "Drop the old flag")
        .unwrap();
    assert_eq!(reused, first, "SQLite reuses the highest id");
    let gaps = store.gap_jobs("maestro").unwrap();
    let seen: Vec<(&str, bool)> = gaps
        .iter()
        .map(|g| (g.request.as_str(), g.dismissed))
        .collect();
    assert_eq!(
        seen,
        [
            ("Drop the old flag", false),
            ("Add the new build flag", true)
        ]
    );

    dashboard.apply(reused, Event::Dismiss);
    store.delete_job(reused, Path::new("/nonexistent")).unwrap();
    let seen: Vec<(String, bool)> = store
        .gap_jobs("maestro")
        .unwrap()
        .into_iter()
        .map(|g| (g.request, g.dismissed))
        .collect();
    assert_eq!(
        seen,
        [
            ("Drop the old flag".to_owned(), true),
            ("Add the new build flag".to_owned(), true)
        ]
    );
}

/// Runs a Renovate job through the packaging analysis, which found `gaps`,
/// and returns the ids of the jobs it proposed.
fn analysed(dashboard: &Dashboard, id: i64, gaps: &[factory_core::Gap]) -> Vec<i64> {
    dashboard.apply(
        id,
        Event::RenovateMerged {
            packaging: true,
            manifest_changed: true,
        },
    );
    dashboard.apply(
        id,
        Event::PackagingGathered(Box::new(factory_core::PackagingContext {
            kind: "snap".into(),
            upstream_repo: "up/app".into(),
            pr_title: String::new(),
            pr_body: String::new(),
            pr_diff: String::new(),
            manifests: Vec::new(),
            from: None,
            to: None,
            upstream: Vec::new(),
            existing: Vec::new(),
        })),
    );
    dashboard.apply(id, Event::Agent(AgentResult::Gaps(gaps.to_vec())));
    let row = dashboard.store.propose_gaps(id, gaps, None).unwrap();
    row.job.renovate_pr().unwrap().gaps.clone().unwrap()
}

#[tokio::test]
async fn a_renovate_job_page_says_no_gaps_were_found() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_renovate_job("maestro", 7, "Update app", "https://example/pull/7")
        .unwrap()
        .unwrap();
    assert_eq!(analysed(&dashboard, id, &[]), Vec::<i64>::new());
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(page.contains("No gaps found"));
}

#[tokio::test]
async fn a_renovate_job_page_links_its_gap_jobs() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_renovate_job("maestro", 7, "Update app", "https://example/pull/7")
        .unwrap()
        .unwrap();
    let gap = factory_core::Gap {
        title: "Add the flag".into(),
        description: "d".into(),
        evidence: "e".into(),
        current: "c".into(),
        suggestion: "s".into(),
    };
    let jobs = analysed(&dashboard, id, &[gap.clone(), gap]);
    assert_eq!(jobs.len(), 2);
    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(!page.contains("No gaps found"));
    for job in jobs {
        assert!(page.contains(&format!("href=\"/jobs/{job}\"")));
    }
}

#[tokio::test]
async fn a_reused_gap_job_id_is_not_linked_from_an_older_result() {
    let dashboard = dashboard();
    let id = dashboard
        .store
        .create_renovate_job("maestro", 7, "Update app", "https://example/pull/7")
        .unwrap()
        .unwrap();
    let gap = factory_core::Gap {
        title: "Add the flag".into(),
        description: "d".into(),
        evidence: "e".into(),
        current: "c".into(),
        suggestion: "s".into(),
    };
    let jobs = analysed(&dashboard, id, &[gap]);
    dashboard
        .store
        .delete_job(jobs[0], Path::new("/nonexistent"))
        .unwrap();
    let reused = dashboard
        .store
        .create_gap_job("maestro", id + 100, 9, "Another gap")
        .unwrap();
    assert_eq!(reused, jobs[0], "SQLite reuses the highest id");

    let page = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(!page.contains(&format!("href=\"/jobs/{reused}\"")));
    assert!(page.contains("no longer available"));
}

/// A dashboard whose `gh` answers `api user` by the `GH_TOKEN` it gets: the
/// token `tok-alice` is the account `alice-bot` and `tok-bob` is `bob-bot`.
fn forge_token_dashboard(secrets_available: bool) -> Dashboard {
    // A closed port, so a read with no token never reaches the real API.
    forge_token_dashboard_at(secrets_available, "http://127.0.0.1:9".to_owned())
}

/// A `glab` that logs each call to `glab-calls` and finds only `team/tool`.
fn fake_salsa(dir: &Path) -> Arc<dyn factory_runner::Forge> {
    let glab = support::executable(
        dir,
        "glab",
        r#"#!/usr/bin/env bash
echo "[$GITLAB_TOKEN] host=$GITLAB_HOST $*" >> "$(dirname "$0")/glab-calls"
if [ "$*" = "api --method GET projects/team%2Ftool --hostname salsa.debian.org" ]; then
  echo '{"permissions":{}}'; exit 0
fi
echo "glab: 404 Not Found (HTTP 404)" >&2
exit 1
"#,
    );
    Arc::new(factory_runner::GitLab {
        glab,
        ..factory_runner::GitLab::default()
    })
}

/// As [`forge_token_dashboard`], with the API a read with no token goes to.
fn forge_token_dashboard_at(secrets_available: bool, api_base: String) -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let gh = |dir: &Path| {
        let gh = support::executable(
            dir,
            "gh",
            r#"#!/usr/bin/env bash
if [ "$1 $2 $3 $4" = "api user --hostname github.com" ]; then
  case "$GH_TOKEN" in
    tok-alice) echo '{"login":"alice-bot","id":11}'; exit 0 ;;
    tok-bob) echo '{"login":"bob-bot","id":22}'; exit 0 ;;
    tok-reader) echo '{"login":"reader-bot","id":33}'; exit 0 ;;
    tok-admin) echo '{"login":"admin-bot","id":44}'; exit 0 ;;
    tok-ghost) echo '{"login":"ghost-bot","id":55}'; exit 0 ;;
    tok-blind) echo '{"login":"blind-bot","id":66}'; exit 0 ;;
    tok-slow) echo '{"login":"slow-bot","id":77}'; exit 0 ;;
    tok-lag)
      here=$(dirname "$0")
      touch "$here/started"
      while [ ! -e "$here/go" ]; do sleep 0.02; done
      echo '{"login":"lag-bot","id":88}'; exit 0 ;;
    *) echo "gh: Bad credentials (HTTP 401)" >&2; exit 1 ;;
  esac
fi
if [ "$1 $2 $3 $4" = "api repos/o/tool --hostname github.com" ]; then
  echo '{"permissions":{"admin":false,"maintain":false,"push":false}}'; exit 0
fi
if [ "$1 $2 $3 $4" = "api repos/o/slowrepo --hostname github.com" ]; then
  here=$(dirname "$0")
  touch "$here/started"
  while [ ! -e "$here/go" ]; do sleep 0.02; done
  echo '{"permissions":{"admin":false,"maintain":false,"push":false}}'; exit 0
fi
if [ "$1 $2 $3 $4" = "api repos/o/secret --hostname github.com" ]; then
  case "$GH_TOKEN" in
    tok-reader) echo '{"permissions":{"admin":false,"maintain":false,"push":false}}'; exit 0 ;;
    tok-bob) echo "gh: Bad credentials (HTTP 401)" >&2; exit 1 ;;
    *) echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
  esac
fi
if [ "$1 $2 $3 $4" = "api repos/Badger-Embedded/maestro --hostname github.com" ]; then
  case "$GH_TOKEN" in
    tok-alice|tok-bob) echo '{"permissions":{"admin":false,"maintain":false,"push":true}}'; exit 0 ;;
    tok-reader) echo '{"permissions":{"admin":false,"maintain":false,"push":false}}'; exit 0 ;;
    tok-admin) echo '{"permissions":{"admin":true,"maintain":true,"push":true}}'; exit 0 ;;
    tok-ghost) echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
    tok-slow)
      here=$(dirname "$0")
      touch "$here/started"
      while [ ! -e "$here/go" ]; do sleep 0.02; done
      echo '{"permissions":{"admin":false,"maintain":false,"push":true}}'; exit 0 ;;
    *) echo "gh: boom" >&2; exit 1 ;;
  esac
fi
echo "fake gh: unexpected: $*" >&2
exit 1
"#,
        );
        GitHub {
            gh,
            api_base,
            ..GitHub::default()
        }
    };
    let secrets: Arc<dyn SecretStore> = if secrets_available {
        Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)))
    } else {
        Arc::new(UnavailableSecrets::new(Arc::clone(&store)))
    };
    dashboard_build(
        store,
        Arc::new(FakeRepos::fit()),
        None,
        |dir| Some(fake_salsa(dir)),
        secrets,
        vec!["boss@example.com".to_owned()],
        gh,
        Vec::new(),
    )
}

const FORGE_PAGE: &str = "/settings/forges/github";

#[tokio::test]
async fn a_non_admin_saves_and_clears_their_forge_token_and_the_page_never_shows_it() {
    let dashboard = forge_token_dashboard(true);
    let page = dashboard.get_as("carol@example.com", FORGE_PAGE).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No token is stored."));
    assert!(page.body.contains(">Forges<"));
    assert!(page.body.contains("href=\"/settings/forges/github\""));

    let saved = forge_post(&dashboard, "carol@example.com", "token", "token=tok-alice").await;
    assert_eq!(saved.status, StatusCode::OK);
    assert!(saved.body.contains("alice-bot"));
    assert!(!saved.body.contains("tok-alice"));
    let page = dashboard.get_as("carol@example.com", FORGE_PAGE).await;
    assert!(page.body.contains("A token is stored."));
    assert!(!page.body.contains("tok-alice"));

    // An empty save keeps the token.
    forge_post(&dashboard, "carol@example.com", "token", "token=").await;
    let owner = ForgeTokenOwner::User {
        login: "carol@example.com",
        forge: &ForgeKind::GitHub,
    };
    assert!(dashboard.store.forge_account(owner).unwrap().is_some());

    let cleared = forge_post(&dashboard, "carol@example.com", "token/clear", "").await;
    assert!(cleared.body.contains("Cleared."));
    assert!(cleared.body.contains("No token is stored."));
    assert!(dashboard.store.forge_account(owner).unwrap().is_none());
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    assert!(secrets.forge_token(owner).unwrap().is_none());
}

#[tokio::test]
async fn a_forge_token_the_forge_rejects_is_not_stored_and_not_echoed() {
    let dashboard = forge_token_dashboard(true);
    let rejected = forge_post(&dashboard, USER, "token", "token=tok-wrong-secret").await;
    assert_eq!(rejected.status, StatusCode::OK);
    assert!(rejected.body.contains("rejected the token"));
    assert!(!rejected.body.contains("tok-wrong-secret"));
    let owner = ForgeTokenOwner::User {
        login: USER,
        forge: &ForgeKind::GitHub,
    };
    assert!(dashboard.store.forge_account(owner).unwrap().is_none());
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    assert!(secrets.forge_token(owner).unwrap().is_none());
}

#[tokio::test]
async fn two_logins_see_only_their_own_forge_token() {
    let dashboard = forge_token_dashboard(true);
    forge_post(&dashboard, "carol@example.com", "token", "token=tok-alice").await;
    forge_post(&dashboard, "dave@example.com", "token", "token=tok-bob").await;

    let carol = dashboard.get_as("carol@example.com", FORGE_PAGE).await.body;
    let dave = dashboard.get_as("dave@example.com", FORGE_PAGE).await.body;
    assert!(carol.contains("alice-bot") && !carol.contains("bob-bot"));
    assert!(dave.contains("bob-bot") && !dave.contains("alice-bot"));

    forge_post(&dashboard, "carol@example.com", "token/clear", "").await;
    let dave = dashboard.get_as("dave@example.com", FORGE_PAGE).await.body;
    assert!(dave.contains("bob-bot"));
}

#[tokio::test]
async fn without_a_master_key_the_forge_page_disables_the_field_and_still_clears() {
    let dashboard = forge_token_dashboard(false);
    let page = dashboard.get_as("carol@example.com", FORGE_PAGE).await.body;
    assert!(page.contains("master key is missing"));
    assert!(page.contains("disabled"));

    let refused = forge_post(&dashboard, "carol@example.com", "token", "token=tok-alice").await;
    assert!(!refused.body.contains("tok-alice"));
    assert!(refused.body.contains("was not saved"));
    let empty = forge_post(&dashboard, "carol@example.com", "token", "token=").await;
    assert!(empty.body.contains("was not saved"));
    assert!(!empty.body.contains("Saved."));
    let owner = ForgeTokenOwner::User {
        login: "carol@example.com",
        forge: &ForgeKind::GitHub,
    };
    assert!(dashboard.store.forge_account(owner).unwrap().is_none());

    dashboard
        .store
        .set_forge_account(
            owner,
            &ForgeAccount {
                login: "old".into(),
                id: 1,
            },
        )
        .unwrap();
    let cleared = forge_post(&dashboard, "carol@example.com", "token/clear", "").await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert!(dashboard.store.forge_account(owner).unwrap().is_none());
}

/// Loads the page as `login`, then posts `form` with the token its form
/// carries, the way the browser does.
async fn forge_post(dashboard: &Dashboard, login: &str, action: &str, form: &str) -> Reply {
    let page = dashboard.get_as(login, FORGE_PAGE).await;
    let doc = scraper::Html::parse_document(&page.body);
    let csrf = doc
        .select(&scraper::Selector::parse("input[name=csrf]").unwrap())
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the form carries a csrf token")
        .to_owned();
    dashboard
        .post_as(
            login,
            &format!("{FORGE_PAGE}/{action}"),
            &format!("{form}&csrf={csrf}"),
        )
        .await
}

const PROJECT_PAGE: &str = "/settings/projects/maestro";
const DAVE: &str = "dave@example.com";

/// Loads the project page as `login`, then posts `form` to `action` under it
/// with the form token the page carries.
async fn project_post(dashboard: &Dashboard, login: &str, action: &str, form: &str) -> Reply {
    let page = dashboard.get_as(login, PROJECT_PAGE).await;
    let doc = scraper::Html::parse_document(&page.body);
    let csrf = doc
        .select(&scraper::Selector::parse("#forge-account input[name=csrf]").unwrap())
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the card carries a csrf token")
        .to_owned();
    dashboard
        .post_as(
            login,
            &format!("{PROJECT_PAGE}/{action}"),
            &format!("{form}&csrf={csrf}"),
        )
        .await
}

fn project_owner() -> ForgeTokenOwner<'static> {
    ForgeTokenOwner::Project {
        project: "maestro",
        forge: &ForgeKind::GitHub,
    }
}

#[tokio::test]
async fn any_collaborator_saves_and_clears_the_project_token() {
    let dashboard = forge_token_dashboard(true);
    dashboard
        .store
        .add_collaborator("maestro", DAVE, "test")
        .unwrap();

    let page = dashboard.get_as(USER, PROJECT_PAGE).await.body;
    assert!(page.contains("Forge account"));
    assert!(page.contains("No token is stored."));
    assert!(page.contains("has no project token"));
    assert!(page.contains("href=\"https://github.com/Badger-Embedded/maestro\""));
    let board = dashboard.get_as(USER, "/").await.body;
    assert!(board.contains("has no project token"));
    assert!(board.contains("/settings/projects/maestro#forge-account"));

    let saved = project_post(&dashboard, USER, "forge-token", "token=tok-alice").await;
    assert_eq!(saved.status, StatusCode::OK);
    assert!(saved.body.contains("alice-bot"));
    assert!(!saved.body.contains("tok-alice"));
    assert!(!saved.body.contains("has no project token"));
    assert!(
        !dashboard
            .get_as(USER, "/")
            .await
            .body
            .contains("has no project token")
    );

    let replaced = project_post(&dashboard, DAVE, "forge-token", "token=tok-bob").await;
    assert!(replaced.body.contains("bob-bot"));
    let account = dashboard
        .store
        .forge_account(project_owner())
        .unwrap()
        .unwrap();
    assert_eq!(account.login, "bob-bot");

    project_post(&dashboard, USER, "forge-token", "token=").await;
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_some()
    );

    let cleared = project_post(&dashboard, USER, "forge-token/clear", "").await;
    assert!(cleared.body.contains("Cleared."));
    assert!(cleared.body.contains("has no project token"));
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    assert!(secrets.forge_token(project_owner()).unwrap().is_none());
}

#[tokio::test]
async fn a_project_token_is_checked_against_the_repository_before_it_is_stored() {
    let dashboard = forge_token_dashboard(true);
    let save = |token: &'static str| {
        let dashboard = &dashboard;
        async move { project_post(dashboard, USER, "forge-token", &format!("token={token}")).await }
    };

    let ghost = save("tok-ghost").await;
    assert!(
        ghost
            .body
            .contains("ghost-bot cannot see Badger-Embedded/maestro")
    );
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );

    let rejected = save("tok-wrong-secret").await;
    assert!(rejected.body.contains("rejected the token"));
    assert!(!rejected.body.contains("tok-wrong-secret"));

    let reader = save("tok-reader").await;
    assert!(reader.body.contains("only Read access"));
    assert!(reader.body.contains("cannot rerun CI or merge"));
    assert!(!reader.body.contains("tok-reader"));
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_some()
    );

    let admin = save("tok-admin").await;
    assert!(admin.body.contains("Admin access"));

    let write = save("tok-alice").await;
    assert!(
        write
            .body
            .contains("Saved. The project token belongs to alice-bot.")
    );
    assert!(!write.body.contains("Read access") && !write.body.contains("Admin access"));

    let blind = save("tok-blind").await;
    assert!(blind.body.contains("blind-bot"));
    assert!(blind.body.contains("was not checked"));
    let account = dashboard
        .store
        .forge_account(project_owner())
        .unwrap()
        .unwrap();
    assert_eq!(account.login, "blind-bot");
}

#[tokio::test]
async fn without_a_master_key_the_project_token_is_refused_and_still_clears() {
    let dashboard = forge_token_dashboard(false);
    let page = dashboard.get_as(USER, PROJECT_PAGE).await.body;
    assert!(page.contains("master key is missing"));
    let refused = project_post(&dashboard, USER, "forge-token", "token=tok-alice").await;
    assert!(refused.body.contains("was not saved"));
    assert!(!refused.body.contains("tok-alice"));
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );

    dashboard
        .store
        .set_forge_account(
            project_owner(),
            &ForgeAccount {
                login: "old".into(),
                id: 1,
            },
        )
        .unwrap();
    let cleared = project_post(&dashboard, USER, "forge-token/clear", "").await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_non_collaborator_cannot_touch_the_project_token_or_a_switch() {
    let dashboard = forge_token_dashboard(true);
    for route in ["forge-token", "forge-token/clear", "use-my-account"] {
        let reply = dashboard
            .post_as(
                STRANGER,
                &format!("{PROJECT_PAGE}/{route}"),
                "token=tok-alice&on=on&csrf=x",
            )
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{route}");
    }
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_collaborator_changes_only_their_own_use_my_account_switch() {
    let dashboard = forge_token_dashboard(true);
    dashboard
        .store
        .add_collaborator("maestro", DAVE, "test")
        .unwrap();
    assert!(!dashboard.store.use_my_account("maestro", USER).unwrap());

    let on = project_post(&dashboard, USER, "use-my-account", "on=on").await;
    assert_eq!(on.status, StatusCode::OK);
    assert!(dashboard.store.use_my_account("maestro", USER).unwrap());
    assert!(!dashboard.store.use_my_account("maestro", DAVE).unwrap());
    assert!(on.body.contains("You have no GitHub token"));
    assert!(on.body.contains("href=\"/settings/forges/github\""));

    // A login field in the form changes nothing about another collaborator.
    project_post(
        &dashboard,
        DAVE,
        "use-my-account",
        &format!("on=on&login={USER}"),
    )
    .await;
    project_post(&dashboard, DAVE, "use-my-account", "").await;
    assert!(dashboard.store.use_my_account("maestro", USER).unwrap());
    assert!(!dashboard.store.use_my_account("maestro", DAVE).unwrap());

    forge_post(&dashboard, USER, "token", "token=tok-alice").await;
    let page = dashboard.get_as(USER, PROJECT_PAGE).await.body;
    assert!(!page.contains("You have no GitHub token"));

    project_post(&dashboard, USER, "use-my-account", "").await;
    assert!(!dashboard.store.use_my_account("maestro", USER).unwrap());

    project_post(&dashboard, DAVE, "use-my-account", "on=on").await;
    dashboard
        .store
        .remove_collaborator("maestro", DAVE)
        .unwrap();
    dashboard
        .store
        .add_collaborator("maestro", DAVE, "test")
        .unwrap();
    assert!(!dashboard.store.use_my_account("maestro", DAVE).unwrap());
}

#[tokio::test]
async fn removing_a_project_deletes_its_token_so_a_reused_name_has_none() {
    let dashboard = forge_token_dashboard(true);
    project_post(&dashboard, USER, "forge-token", "token=tok-alice").await;
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_some()
    );

    let removed = dashboard
        .post(&format!("{PROJECT_PAGE}/remove"), "confirm=maestro")
        .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    assert!(secrets.forge_token(project_owner()).unwrap().is_none());

    dashboard
        .store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some(USER),
        )
        .unwrap();
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );
    assert!(secrets.forge_token(project_owner()).unwrap().is_none());
}

/// Runs a project-token save of `tok-slow` and, while its access check waits,
/// runs `meanwhile`. Returns the save's reply.
async fn save_while(dashboard: &Dashboard, meanwhile: impl FnOnce()) -> Reply {
    let started = dashboard.data.path().join("started");
    let go = dashboard.data.path().join("go");
    let save = project_post(dashboard, USER, "forge-token", "token=tok-slow");
    let driver = async {
        while !started.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        meanwhile();
        std::fs::write(&go, "").unwrap();
    };
    tokio::join!(save, driver).0
}

#[tokio::test]
async fn a_save_that_finishes_after_the_project_was_removed_stores_nothing() {
    let dashboard = forge_token_dashboard(true);
    dashboard
        .store
        .add_collaborator("maestro", DAVE, "test")
        .unwrap();
    let reply = save_while(&dashboard, || {
        dashboard.store.remove_project("maestro").unwrap();
        dashboard
            .store
            .create_project(
                "maestro",
                &ForgeKind::GitHub,
                "Badger-Embedded/maestro",
                "main",
                None,
                Some(USER),
            )
            .unwrap();
        dashboard.store.project_with_instance("maestro").unwrap();
    })
    .await;
    assert!(reply.body.contains("Nothing was saved"));
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    assert!(secrets.forge_token(project_owner()).unwrap().is_none());
}

#[tokio::test]
async fn removing_another_project_meanwhile_does_not_stop_a_save() {
    let dashboard = forge_token_dashboard(true);
    dashboard
        .store
        .create_project(
            "other",
            &ForgeKind::GitHub,
            "Badger-Embedded/other",
            "main",
            None,
            Some(USER),
        )
        .unwrap();
    let reply = save_while(&dashboard, || {
        dashboard.store.remove_project("other").unwrap();
    })
    .await;
    assert!(
        reply
            .body
            .contains("Saved. The project token belongs to slow-bot.")
    );
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_save_by_a_login_removed_meanwhile_stores_nothing() {
    let dashboard = forge_token_dashboard(true);
    dashboard
        .store
        .add_collaborator("maestro", DAVE, "test")
        .unwrap();
    let reply = save_while(&dashboard, || {
        dashboard
            .store
            .remove_collaborator("maestro", USER)
            .unwrap();
    })
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(
        dashboard
            .store
            .forge_account(project_owner())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn forge_token_requests_without_a_valid_form_token_change_nothing() {
    let dashboard = forge_token_dashboard(true);
    let owner = ForgeTokenOwner::User {
        login: "carol@example.com",
        forge: &ForgeKind::GitHub,
    };
    let secrets = LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone());
    let other = dashboard.get_as("dave@example.com", FORGE_PAGE).await.body;
    let other_token = other
        .split("name=\"csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();

    for bad in [
        "",
        "&csrf=",
        "&csrf=forged",
        &format!("&csrf={other_token}"),
    ] {
        let saved = dashboard
            .post_as(
                "carol@example.com",
                &format!("{FORGE_PAGE}/token"),
                &format!("token=tok-alice{bad}"),
            )
            .await;
        assert_eq!(saved.status, StatusCode::FORBIDDEN, "{bad}");
        assert!(dashboard.store.forge_account(owner).unwrap().is_none());
        assert!(secrets.forge_token(owner).unwrap().is_none());
    }

    forge_post(&dashboard, "carol@example.com", "token", "token=tok-alice").await;
    for bad in ["", "csrf=forged", &format!("csrf={other_token}")] {
        let cleared = dashboard
            .post_as(
                "carol@example.com",
                &format!("{FORGE_PAGE}/token/clear"),
                bad,
            )
            .await;
        assert_eq!(cleared.status, StatusCode::FORBIDDEN, "{bad}");
        assert!(dashboard.store.forge_account(owner).unwrap().is_some());
        assert!(secrets.forge_token(owner).unwrap().is_some());
    }
    forge_post(&dashboard, "carol@example.com", "token/clear", "").await;
    assert!(secrets.forge_token(owner).unwrap().is_none());
}

#[tokio::test]
async fn an_unknown_forge_is_a_404() {
    let dashboard = forge_token_dashboard(true);
    assert_eq!(
        dashboard.get("/settings/forges/other").await.status,
        StatusCode::NOT_FOUND
    );
    let post = dashboard
        .post("/settings/forges/other/token", "token=x")
        .await;
    assert_eq!(post.status, StatusCode::NOT_FOUND);
}

async fn checks_post(dashboard: &Dashboard, login: &str, checks: &str) -> Reply {
    let page = dashboard.get_as(login, PROJECT_PAGE).await;
    let doc = scraper::Html::parse_document(&page.body);
    let csrf = doc
        .select(&scraper::Selector::parse("form[action$='/checks'] input[name=csrf]").unwrap())
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the Local checks card carries a csrf token")
        .to_owned();
    dashboard
        .post_as(
            login,
            &format!("{PROJECT_PAGE}/checks"),
            &format!("checks={checks}&csrf={csrf}"),
        )
        .await
}

fn checks_field(body: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(body);
    let form = scraper::Selector::parse("form[action$='/checks'] input[name=checks]").unwrap();
    let input = doc.select(&form).next()?;
    Some(input.value().attr("value").unwrap_or("").to_owned())
}

fn stored_checks(dashboard: &Dashboard, project: &str) -> Option<String> {
    dashboard.store.project(project).unwrap().unwrap().checks
}

#[tokio::test]
async fn the_checks_card_shows_the_stored_command_or_an_empty_field() {
    let dashboard = dashboard();
    let page = dashboard.get(PROJECT_PAGE).await.body;
    assert!(page.contains("<h2>Local checks</h2>"));
    assert_eq!(checks_field(&page).as_deref(), Some(""));

    dashboard
        .store
        .save_project_checks("maestro", Some("just lint"))
        .unwrap();
    let page = dashboard.get(PROJECT_PAGE).await.body;
    assert_eq!(checks_field(&page).as_deref(), Some("just lint"));
}

#[tokio::test]
async fn the_checks_field_has_the_onboarding_label_and_hint() {
    let dashboard = dashboard();
    let onboard = dashboard.get("/onboard").await.body;
    let settings = dashboard.get(PROJECT_PAGE).await.body;
    let label = "Local checks command (optional)";
    let hint = "before it pushes, such as nix develop -c just lint &amp;&amp; nix develop -c just test. It is separate from GitHub";
    let end = "CI on the pull request. Leave it empty to push without local checks.";
    for body in [&onboard, &settings] {
        assert!(body.contains(label));
        assert!(body.contains(hint), "{hint}");
        assert!(body.contains(end), "{end}");
    }
}

#[tokio::test]
async fn saving_the_checks_card_stores_a_trimmed_command_and_redraws_the_page() {
    let dashboard = dashboard();
    let reply = checks_post(&dashboard, USER, "++just+lint+%26%26+just+test++").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    assert_eq!(
        stored_checks(&dashboard, "maestro").as_deref(),
        Some("just lint && just test")
    );
    assert_eq!(
        checks_field(&reply.body).as_deref(),
        Some("just lint && just test")
    );
}

#[tokio::test]
async fn an_empty_or_blank_checks_save_stores_no_command() {
    let dashboard = dashboard();
    for blank in ["", "+++"] {
        dashboard
            .store
            .save_project_checks("maestro", Some("just lint"))
            .unwrap();
        let reply = checks_post(&dashboard, USER, blank).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(stored_checks(&dashboard, "maestro"), None, "{blank:?}");
        assert_eq!(checks_field(&reply.body).as_deref(), Some(""));
    }
}

#[tokio::test]
async fn a_non_collaborator_cannot_save_the_checks_command() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_checks("maestro", Some("just lint"))
        .unwrap();
    let reply = dashboard
        .post_as(STRANGER, "/settings/projects/maestro/checks", "checks=evil")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let unknown = dashboard
        .post_as(STRANGER, "/settings/projects/nope/checks", "checks=evil")
        .await;
    assert_eq!(unknown.status, reply.status);
    assert_eq!(
        stored_checks(&dashboard, "maestro").as_deref(),
        Some("just lint")
    );
}

async fn packages_post(dashboard: &Dashboard, login: &str, packages: &str) -> Reply {
    let page = dashboard.get_as(login, PROJECT_PAGE).await;
    let doc = scraper::Html::parse_document(&page.body);
    let csrf = doc
        .select(&scraper::Selector::parse("form[action$='/packages'] input[name=csrf]").unwrap())
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the Worker packages card carries a csrf token")
        .to_owned();
    dashboard
        .post_as(
            login,
            &format!("{PROJECT_PAGE}/packages"),
            &format!("packages={packages}&csrf={csrf}"),
        )
        .await
}

#[tokio::test]
async fn the_worker_packages_card_saves_validates_and_shows_the_list() {
    let dashboard = dashboard();
    let page = dashboard.get(PROJECT_PAGE).await.body;
    assert!(page.contains("<h2>Worker packages</h2>"));

    let reply = packages_post(
        &dashboard,
        USER,
        "playwright-mcp%0D%0Apython3Packages.requests%0A",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let stored = vec![
        "playwright-mcp".to_owned(),
        "python3Packages.requests".to_owned(),
    ];
    assert_eq!(dashboard.store.project_packages("maestro").unwrap(), stored);
    let page = dashboard.get(PROJECT_PAGE).await.body;
    assert!(page.contains("playwright-mcp\npython3Packages.requests"));

    for bad in ["foo%3Brm+-rf+%2F", "nixpkgs%23foo"] {
        let reply = packages_post(&dashboard, USER, bad).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(dashboard.store.project_packages("maestro").unwrap(), stored);
    }
}

#[tokio::test]
async fn worker_packages_collapse_duplicates_and_have_count_and_length_limits() {
    let dashboard = dashboard();
    let reply = packages_post(&dashboard, USER, "jq%0A%0A+jq+%0Ayq").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        dashboard.store.project_packages("maestro").unwrap(),
        ["jq", "yq"]
    );
    let many: Vec<String> = (0..33).map(|n| format!("p{n}")).collect();
    for bad in [many.join("%0A"), "a".repeat(129)] {
        let reply = packages_post(&dashboard, USER, &bad).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }
    let ok = packages_post(&dashboard, USER, &"a".repeat(128)).await;
    assert_eq!(ok.status, StatusCode::OK);
}

#[tokio::test]
async fn a_collaborator_of_another_project_cannot_save_worker_packages() {
    let dashboard = two_projects();
    dashboard
        .store
        .add_collaborator("tool", DAVE, "test")
        .unwrap();
    let reply = dashboard
        .post_as(
            DAVE,
            "/settings/projects/maestro/packages",
            "packages=evil&csrf=x",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        dashboard.store.project_packages("maestro").unwrap(),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn checks_requests_without_a_valid_form_token_change_nothing() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_checks("maestro", Some("just lint"))
        .unwrap();
    dashboard
        .store
        .add_collaborator("maestro", DAVE, "test")
        .unwrap();
    let other = dashboard.get_as(DAVE, PROJECT_PAGE).await.body;
    let other_token = scraper::Html::parse_document(&other)
        .select(&scraper::Selector::parse("form[action$='/checks'] input[name=csrf]").unwrap())
        .next()
        .and_then(|input| input.value().attr("value"))
        .unwrap()
        .to_owned();
    for bad in [
        "",
        "&csrf=",
        "&csrf=forged",
        &format!("&csrf={other_token}"),
    ] {
        let reply = dashboard
            .post(
                "/settings/projects/maestro/checks",
                &format!("checks=evil{bad}"),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{bad}");
        assert_eq!(
            stored_checks(&dashboard, "maestro").as_deref(),
            Some("just lint")
        );
    }
}

fn seed_non_default_settings(dashboard: &Dashboard) {
    let store = &dashboard.store;
    store
        .save_project_checks("maestro", Some("just lint"))
        .unwrap();
    store
        .create_project(
            "other",
            &ForgeKind::GitHub,
            "o/other",
            "main",
            Some("make"),
            Some("alice"),
        )
        .unwrap();
    store.add_collaborator("maestro", DAVE, "test").unwrap();
    store
        .save_project_models("maestro", &models(Some("opus"), Some("sonnet")))
        .unwrap();
    store.save_project_settings("maestro", true, true).unwrap();
    store
        .save_project_gates(
            "maestro",
            Gates {
                spec: false,
                plan: false,
                publish: true,
            },
        )
        .unwrap();
    let maintenance = factory_server::Maintenance {
        renovate: true,
        renovate_author: None,
        packaging: None,
    };
    store
        .save_project_maintenance("maestro", &maintenance)
        .unwrap();
    let project_token = ForgeTokenOwner::Project {
        project: "maestro",
        forge: &ForgeKind::GitHub,
    };
    store
        .set_forge_account(
            project_token,
            &ForgeAccount {
                login: "bot".into(),
                id: 7,
            },
        )
        .unwrap();
    store.set_use_my_account("maestro", DAVE, true).unwrap();
    let mcp = McpProjectSettings {
        enabled: true,
        stages: factory_server::McpStages {
            plan: true,
            ..Default::default()
        },
    };
    store.save_project_mcp("maestro", "context7", &mcp).unwrap();
}

#[tokio::test]
async fn the_checks_card_and_the_other_settings_forms_leave_each_other_alone() {
    let dashboard = dashboard();
    seed_non_default_settings(&dashboard);
    let store = &dashboard.store;
    let project_token = ForgeTokenOwner::Project {
        project: "maestro",
        forge: &ForgeKind::GitHub,
    };

    let snapshot = || {
        (
            store
                .project_models("maestro", &StageModels::default())
                .unwrap(),
            store.project_auto_merge("maestro", false).unwrap(),
            store.project_llm_review("maestro", false).unwrap(),
            store.project_gates("maestro").unwrap(),
            store.collaborators("maestro").unwrap(),
            store.project_maintenance("maestro").unwrap(),
            store.forge_account(project_token).unwrap(),
            store.use_my_account("maestro", DAVE).unwrap(),
            store.project_mcp("maestro", "context7").unwrap(),
        )
    };
    let before = snapshot();
    assert!(before.1 && before.2 && before.7 && before.8.enabled);

    let reply = checks_post(&dashboard, DAVE, "just+test").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        stored_checks(&dashboard, "maestro").as_deref(),
        Some("just test")
    );
    assert_eq!(stored_checks(&dashboard, "other").as_deref(), Some("make"));
    assert_eq!(snapshot(), before);

    let reply = dashboard
        .post("/settings/projects/maestro/maintenance", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!store.project_maintenance("maestro").unwrap().renovate);
    assert_eq!(
        stored_checks(&dashboard, "maestro").as_deref(),
        Some("just test")
    );

    let form = "clarify=haiku&spec=&plan=&implement=&review=&\
        clarify_effort=medium&spec_effort=medium&plan_effort=medium&\
        implement_effort=medium&review_effort=medium&\
        clarify_agent=claude&spec_agent=claude&plan_agent=claude&\
        implement_agent=claude&review_agent=claude";
    let reply = dashboard.post("/settings/projects/maestro", form).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        store
            .project_models("maestro", &StageModels::default())
            .unwrap()
            .clarify
            .as_deref(),
        Some("haiku")
    );
    assert_eq!(
        stored_checks(&dashboard, "maestro").as_deref(),
        Some("just test")
    );
}

#[test]
fn the_checks_command_round_trips_through_the_store() {
    let store = Store::open_in_memory().unwrap();
    for name in ["a", "b"] {
        store
            .create_project(
                name,
                &ForgeKind::GitHub,
                &format!("o/{name}"),
                "main",
                Some("make"),
                None,
            )
            .unwrap();
    }
    store.save_project_checks("a", Some("just lint")).unwrap();
    assert_eq!(
        store.project("a").unwrap().unwrap().checks.as_deref(),
        Some("just lint")
    );
    assert_eq!(
        store.project("b").unwrap().unwrap().checks.as_deref(),
        Some("make")
    );
    store.save_project_checks("a", None).unwrap();
    assert_eq!(store.project("a").unwrap().unwrap().checks, None);
}

async fn name_post(dashboard: &Dashboard, login: &str, value: &str) -> Reply {
    let page = dashboard.get_as(login, PROJECT_PAGE).await;
    let doc = scraper::Html::parse_document(&page.body);
    let csrf = doc
        .select(&scraper::Selector::parse("form[action$='/name'] input[name=csrf]").unwrap())
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the Project name card carries a csrf token")
        .to_owned();
    dashboard
        .post_as(
            login,
            &format!("{PROJECT_PAGE}/name"),
            &format!("display_name={value}&csrf={csrf}"),
        )
        .await
}

#[test]
fn the_store_reads_and_writes_a_display_name() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_project("b", &ForgeKind::GitHub, "o/b", "main", None, Some("alice"))
        .unwrap();
    store
        .create_project("a", &ForgeKind::GitHub, "o/a", "main", None, Some("alice"))
        .unwrap();
    assert_eq!(store.project_shown_name("b").unwrap(), "b");
    store.save_project_display_name("b", Some("Alpha")).unwrap();
    assert_eq!(store.project_shown_name("b").unwrap(), "Alpha");
    let labels = store.project_labels_for("alice").unwrap();
    let pairs: Vec<_> = labels
        .iter()
        .map(|l| (l.id.as_str(), l.shown.as_str()))
        .collect();
    assert_eq!(pairs, [("b", "Alpha"), ("a", "a")]);
    assert!(store.project("b").unwrap().is_some());
    assert!(matches!(
        store.save_project_display_name("a", Some("Alpha")),
        Err(StoreError::DuplicateName(_))
    ));
    assert!(matches!(
        store.save_project_display_name("a", Some("b")),
        Err(StoreError::DuplicateName(_))
    ));
    store.save_project_display_name("b", None).unwrap();
    assert_eq!(store.project_shown_name("b").unwrap(), "b");
}

#[tokio::test]
async fn saving_a_display_name_shows_it_and_keeps_the_identifier() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let reply = name_post(&dashboard, USER, "++My+Project++").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some("/settings/projects/maestro?notice=Saved.")
    );
    let page = dashboard.get(PROJECT_PAGE).await.body;
    assert!(page.contains("<h1>My Project</h1>"));
    assert!(page.contains("Remove My Project"));
    assert!(page.contains(r#"value="My Project""#));
    let board = dashboard.get("/").await;
    assert_current_project(&board.body, "maestro", "My Project");
    assert!(board.body.contains("Describe a change for My Project…"));
    let job = dashboard.get(&format!("/jobs/{id}")).await.body;
    assert!(job.contains("My Project"));
    assert!(dashboard.store.project("maestro").unwrap().is_some());
    assert_eq!(
        dashboard.store.get(id).unwrap().job.stage(),
        &Stage::Requested
    );
}

#[tokio::test]
async fn an_empty_value_or_the_identifier_clears_the_display_name() {
    let dashboard = dashboard();
    for clear in ["", "maestro", "+maestro+"] {
        dashboard
            .store
            .save_project_display_name("maestro", Some("Shown"))
            .unwrap();
        let reply = name_post(&dashboard, USER, clear).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{clear:?}");
        assert_eq!(
            dashboard.store.project_shown_name("maestro").unwrap(),
            "maestro"
        );
    }
}

#[tokio::test]
async fn a_bad_display_name_is_refused_and_changes_nothing() {
    let dashboard = dashboard();
    dashboard
        .store
        .create_project(
            "tool",
            &ForgeKind::GitHub,
            "o/tool",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    dashboard
        .store
        .save_project_display_name("tool", Some("Other Tool"))
        .unwrap();
    dashboard
        .store
        .save_project_display_name("maestro", Some("Keep"))
        .unwrap();
    let long = "x".repeat(65);
    for (value, needle) in [
        (long.as_str(), "at most 64"),
        ("a%07b", "control characters"),
        ("tool", "already exists."),
        ("Other+Tool", "already exists."),
    ] {
        let reply = name_post(&dashboard, USER, value).await;
        assert_eq!(reply.status, StatusCode::OK, "{value}");
        assert!(reply.body.contains(needle), "{value}");
        assert_eq!(
            dashboard.store.project_shown_name("maestro").unwrap(),
            "Keep"
        );
    }
    let sixty_four = "é".repeat(64);
    assert_eq!(
        name_post(&dashboard, USER, &sixty_four).await.status,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn a_non_collaborator_cannot_save_a_display_name() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_as(
            STRANGER,
            "/settings/projects/maestro/name",
            "display_name=evil",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let unknown = dashboard
        .post_as(
            STRANGER,
            "/settings/projects/nope/name",
            "display_name=evil",
        )
        .await;
    assert_eq!(unknown.status, reply.status);
    let forged = dashboard
        .post(
            "/settings/projects/maestro/name",
            "display_name=evil&csrf=forged",
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert_eq!(
        dashboard.store.project_shown_name("maestro").unwrap(),
        "maestro"
    );
}

#[tokio::test]
async fn onboarding_refuses_an_identifier_equal_to_a_display_name() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "widgets",
            &ForgeKind::GitHub,
            "acme/old",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    store
        .save_project_display_name("widgets", Some("shiny"))
        .unwrap();
    rejected(
        Arc::new(FakeRepos::fit()),
        store,
        "repo=acme%2Fnew&name=shiny&action=create",
        "already exists",
    )
    .await;
}

#[tokio::test]
async fn removal_asks_for_the_shown_name_and_a_reonboarded_project_has_none() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_display_name("maestro", Some("My Project"))
        .unwrap();
    let page = dashboard.get(PROJECT_PAGE).await.body;
    assert!(page.contains("placeholder=\"My Project\""));
    assert!(page.contains("Remove My Project"));
    let refused = dashboard
        .post("/settings/projects/maestro/remove", "confirm=maestro")
        .await;
    assert!(refused.body.contains("Nothing was removed."));
    assert!(dashboard.store.project("maestro").unwrap().is_some());
    dashboard
        .post("/settings/projects/maestro/remove", "confirm=My+Project")
        .await;
    assert!(dashboard.store.project("maestro").unwrap().is_none());
    dashboard
        .store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "o/new",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    assert_eq!(
        dashboard.store.project_shown_name("maestro").unwrap(),
        "maestro"
    );
}

#[tokio::test]
async fn the_sidebar_orders_projects_by_shown_name_and_mcp_pages_show_it() {
    let dashboard = two_projects();
    add_context7(&dashboard).await;
    dashboard
        .store
        .save_project_display_name("maestro", Some("zed"))
        .unwrap();
    let page = dashboard.get(PROJECT_PAGE).await.body;
    let nav = &page[page.find("<nav class=\"settings-nav\"").unwrap()..];
    assert!(
        nav.find("href=\"/settings/projects/tool\"").unwrap()
            < nav.find("href=\"/settings/projects/maestro\"").unwrap()
    );
    let mcp = dashboard
        .get("/settings/projects/maestro/mcp/context7")
        .await
        .body;
    assert!(mcp.contains("zed"));
    let server = dashboard.get("/settings/mcp/context7").await.body;
    let used = &server[server.find("Used by").unwrap()..];
    assert!(used.contains("zed"));
    assert!(used.contains(r#"href="/settings/projects/maestro/mcp/context7""#));
}

#[tokio::test]
async fn a_refused_onboarding_keeps_the_retained_jobs_of_the_name() {
    let dashboard = dashboard();
    seed_user_token(&dashboard.store, "bob@example.com");
    let id = dashboard.new_job().await;
    dashboard
        .post("/settings/projects/maestro/remove", "confirm=maestro")
        .await;
    dashboard
        .store
        .create_project(
            "widgets",
            &ForgeKind::GitHub,
            "acme/widgets",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    dashboard
        .store
        .save_project_display_name("widgets", Some("maestro"))
        .unwrap();
    let reply = dashboard
        .post_as(
            "bob@example.com",
            "/onboard",
            "repo=acme%2Fmaestro&name=maestro&action=skip",
        )
        .await;
    assert!(reply.body.contains("already exists"));
    assert!(dashboard.store.get(id).is_ok());
}
#[tokio::test]
async fn a_display_name_save_waits_for_a_running_onboarding() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "acme/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    seed_user_token(&store, "bob@example.com");
    let dashboard = dashboard_with(Arc::clone(&store));
    let pass = dashboard.slots.lock_pass().await;
    let onboard = dashboard.post_as(
        "bob@example.com",
        "/onboard",
        "repo=acme%2Ffresh&name=fresh&action=skip",
    );
    let save = name_post(&dashboard, USER, "fresh");
    tokio::pin!(onboard, save);
    // Onboarding stops at the held pass lock, after its name check.
    for _ in 0..20 {
        let polled = std::future::poll_fn(|cx| std::task::Poll::Ready(onboard.as_mut().poll(cx)));
        assert!(polled.await.is_pending());
        tokio::task::yield_now().await;
    }
    for _ in 0..20 {
        let polled = std::future::poll_fn(|cx| std::task::Poll::Ready(save.as_mut().poll(cx)));
        assert!(polled.await.is_pending(), "the save ran during onboarding");
        tokio::task::yield_now().await;
    }
    assert!(store.project("fresh").unwrap().is_none());
    drop(pass);
    let (onboarded, saved) = tokio::join!(onboard, save);
    assert_eq!(onboarded.status, StatusCode::SEE_OTHER);
    assert!(saved.body.contains("already exists"));
    assert_eq!(store.project_shown_name("maestro").unwrap(), "maestro");
}

const UPSTREAM_FORM: &str = "renovate=on&packaging=on&packaging_type=deb&manifest_files=debian%2Fcontrol\
    &build_config_files=&upstream_forge=github";

async fn upstream_post(dashboard: &Dashboard, action: &str, form: &str) -> Reply {
    let page = dashboard.get(PROJECT_PAGE).await;
    let doc = scraper::Html::parse_document(&page.body);
    let csrf = doc
        .select(
            &scraper::Selector::parse("form[action$=\"/maintenance\"] input[name=csrf]").unwrap(),
        )
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the maintenance form carries a csrf token")
        .to_owned();
    dashboard
        .post(
            &format!("{PROJECT_PAGE}/upstream-token{action}?forge=github"),
            &format!("{UPSTREAM_FORM}&upstream_repo=o%2Ftool&{form}&csrf={csrf}"),
        )
        .await
}

#[tokio::test]
async fn check_connection_names_the_credential_and_stores_nothing() {
    let dashboard = dashboard_with_project_token();
    let form = format!("{UPSTREAM_FORM}&upstream_repo=o%2Ftool");
    let before = dashboard.store.project_maintenance("maestro").unwrap();

    let reply = dashboard
        .post(&format!("{PROJECT_PAGE}/maintenance/check"), &form)
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .body
            .contains("Checked with the project token of alice-bot: o/tool is readable on GitHub.")
    );
    assert!(!reply.body.contains("tok-alice"));
    assert_eq!(
        dashboard.store.project_maintenance("maestro").unwrap(),
        before
    );
}

#[tokio::test]
async fn an_upstream_token_replaces_the_project_token_for_the_check_and_is_never_shown() {
    let dashboard = dashboard_with_project_token();
    let secret = format!("{UPSTREAM_FORM}&upstream_repo=o%2Fsecret");

    let refused = dashboard
        .post(&format!("{PROJECT_PAGE}/maintenance"), &secret)
        .await;
    assert!(
        refused
            .body
            .contains("o/secret could not be read on GitHub")
    );
    assert!(refused.body.contains("Save an upstream token"));
    assert!(refused.body.contains("Nothing was saved."));
    assert!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .is_none()
    );

    dashboard
        .post(
            &format!("{PROJECT_PAGE}/maintenance"),
            &format!("{UPSTREAM_FORM}&upstream_repo=o%2Ftool"),
        )
        .await;
    let rejected = upstream_post(&dashboard, "", "token_github=nope").await;
    assert!(
        rejected
            .body
            .contains("GitHub rejected the token. Nothing was saved.")
    );
    assert!(!rejected.body.contains("nope"));

    let saved = upstream_post(&dashboard, "", "token_github=tok-reader").await;
    assert!(saved.body.contains("reader-bot"));
    assert!(!saved.body.contains("tok-reader"));

    let checked = dashboard
        .post(&format!("{PROJECT_PAGE}/maintenance/check"), &secret)
        .await;
    assert!(checked.body.contains(
        "Checked with the upstream token of reader-bot: o/secret is readable on GitHub."
    ));
    let saved = dashboard
        .post(&format!("{PROJECT_PAGE}/maintenance"), &secret)
        .await;
    assert!(saved.body.contains("Saved."));
    assert_eq!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .unwrap()
            .upstream_repo,
        "o/secret"
    );

    let cleared = upstream_post(&dashboard, "/clear", "").await;
    assert!(cleared.body.contains("Cleared."));
}

#[tokio::test]
async fn a_read_with_no_token_is_checked_and_a_failed_check_does_not_stop_the_save() {
    let (api, _hits) = stub_api().await;
    let dashboard = forge_token_dashboard_at(true, api);
    seed_project_token(&dashboard, "tok-alice", "alice-bot", 11);
    // The upstream is on Salsa, so the GitHub project token does not apply.
    let on_salsa = format!("{UPSTREAM_FORM}&upstream_repo=team%2Ftool").replace("github", "salsa");
    let reply = dashboard
        .post(&format!("{PROJECT_PAGE}/maintenance/check"), &on_salsa)
        .await;
    assert!(reply.body.contains("Checked with no token: team/tool"));
    let calls = std::fs::read_to_string(dashboard.data.path().join("glab-calls")).unwrap();
    assert_eq!(
        calls.trim(),
        "[] host=salsa.debian.org api --method GET projects/team%2Ftool --hostname salsa.debian.org"
    );

    let none = format!("{UPSTREAM_FORM}&upstream_repo=o%2Ftool");
    let reply = dashboard
        .post(&format!("{PROJECT_PAGE}/maintenance/check"), &none)
        .await;
    assert!(
        reply
            .body
            .contains("Checked with the project token of alice-bot")
    );

    // With no project token the read is unauthenticated and rate limited.
    dashboard
        .store
        .clear_forge_account(project_owner())
        .unwrap();
    let reply = dashboard
        .post(&format!("{PROJECT_PAGE}/maintenance/check"), &none)
        .await;
    assert!(reply.body.contains(
        "Checked with no token: o/tool is readable on GitHub. GitHub rate limits reads with no token"
    ));
}

#[tokio::test]
async fn saving_packaging_when_the_upstream_check_cannot_run_still_saves() {
    let dashboard = dashboard_with_project_token();
    let reply = dashboard
        .post(
            &format!("{PROJECT_PAGE}/maintenance"),
            &format!("{UPSTREAM_FORM}&upstream_repo=o%2Fboom"),
        )
        .await;
    assert!(reply.body.contains("Saved."));
    assert!(reply.body.contains("The upstream was not checked"));
    assert!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .is_some()
    );
}

async fn upstream_csrf(dashboard: &Dashboard) -> String {
    let page = dashboard.get(PROJECT_PAGE).await;
    scraper::Html::parse_document(&page.body)
        .select(
            &scraper::Selector::parse("form[action$=\"/maintenance\"] input[name=csrf]").unwrap(),
        )
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the maintenance form carries a csrf token")
        .to_owned()
}

/// Posts `uri` and runs `meanwhile` once the fake `gh` is blocked in the call
/// that the post made.
async fn post_while(
    dashboard: &Dashboard,
    uri: &str,
    form: &str,
    meanwhile: impl FnOnce(),
) -> Reply {
    let started = dashboard.data.path().join("started");
    let go = dashboard.data.path().join("go");
    let post = dashboard.post(uri, form);
    let driver = async {
        for _ in 0..500 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(started.exists(), "the forge call did not start within 5 s");
        meanwhile();
        std::fs::write(&go, "").unwrap();
    };
    tokio::join!(post, driver).0
}

fn upstream_owner() -> ForgeTokenOwner<'static> {
    ForgeTokenOwner::Upstream {
        project: "maestro",
        forge: &ForgeKind::GitHub,
    }
}

#[tokio::test]
async fn an_upstream_token_saved_after_the_project_was_recreated_stores_nothing() {
    let dashboard = dashboard_with_project_token();
    let csrf = upstream_csrf(&dashboard).await;
    let form = format!("{UPSTREAM_FORM}&upstream_repo=o%2Ftool&token_github=tok-lag&csrf={csrf}");

    let reply = post_while(
        &dashboard,
        &format!("{PROJECT_PAGE}/upstream-token?forge=github"),
        &form,
        || {
            dashboard.store.remove_project("maestro").unwrap();
            dashboard
                .store
                .create_project(
                    "maestro",
                    &ForgeKind::GitHub,
                    "Badger-Embedded/maestro",
                    "main",
                    None,
                    Some(USER),
                )
                .unwrap();
            dashboard.store.project_with_instance("maestro").unwrap();
        },
    )
    .await;

    assert!(reply.body.contains("The project changed"));
    assert!(
        dashboard
            .store
            .forge_account(upstream_owner())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn an_upstream_token_saved_by_a_login_removed_meanwhile_stores_nothing() {
    let dashboard = dashboard_with_project_token();
    let csrf = upstream_csrf(&dashboard).await;
    let form = format!("{UPSTREAM_FORM}&upstream_repo=o%2Ftool&token_github=tok-lag&csrf={csrf}");

    let reply = post_while(
        &dashboard,
        &format!("{PROJECT_PAGE}/upstream-token?forge=github"),
        &form,
        || {
            dashboard
                .store
                .remove_collaborator("maestro", USER)
                .unwrap();
        },
    )
    .await;

    assert_ne!(reply.status, StatusCode::OK);
    assert!(
        dashboard
            .store
            .forge_account(upstream_owner())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_maintenance_save_refuses_a_result_checked_before_an_upstream_token_appeared() {
    let dashboard = dashboard_with_project_token();
    let form = format!("{UPSTREAM_FORM}&upstream_repo=o%2Fslowrepo");

    let reply = post_while(
        &dashboard,
        &format!("{PROJECT_PAGE}/maintenance"),
        &form,
        || {
            LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
                .put_forge_token(
                    upstream_owner(),
                    "tok-reader",
                    &ForgeAccount {
                        login: "reader-bot".into(),
                        id: 33,
                    },
                )
                .unwrap();
        },
    )
    .await;

    assert!(
        reply
            .body
            .contains("A token changed while the access was checked")
    );
    assert!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .is_none()
    );
}

#[tokio::test]
async fn a_maintenance_save_refuses_a_result_checked_with_an_upstream_token_since_cleared() {
    let dashboard = dashboard_with_project_token();
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .put_forge_token(
            upstream_owner(),
            "tok-reader",
            &ForgeAccount {
                login: "reader-bot".into(),
                id: 33,
            },
        )
        .unwrap();
    let form = format!("{UPSTREAM_FORM}&upstream_repo=o%2Fslowrepo");

    let reply = post_while(
        &dashboard,
        &format!("{PROJECT_PAGE}/maintenance"),
        &form,
        || {
            LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
                .delete_forge_token(upstream_owner())
                .unwrap();
        },
    )
    .await;
    assert!(
        reply
            .body
            .contains("A token changed while the access was checked")
    );
    assert!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .is_none()
    );
}

#[tokio::test]
async fn a_maintenance_save_ignores_an_upstream_token_stored_for_the_other_forge() {
    let dashboard = dashboard_with_project_token();
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .put_forge_token(
            ForgeTokenOwner::Upstream {
                project: "maestro",
                forge: &ForgeKind::Salsa,
            },
            "tok-salsa",
            &ForgeAccount {
                login: "salsa-bot".into(),
                id: 9,
            },
        )
        .unwrap();

    let csrf = upstream_csrf(&dashboard).await;
    let reply = dashboard
        .post(
            &format!("{PROJECT_PAGE}/maintenance"),
            &format!("{UPSTREAM_FORM}&upstream_repo=o%2Ftool&csrf={csrf}"),
        )
        .await;

    assert!(!reply.body.contains("A token changed"));
    assert!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .is_some()
    );
}

#[tokio::test]
async fn an_upstream_token_button_acts_on_the_forge_it_names_and_an_unknown_forge_is_refused() {
    let dashboard = dashboard_with_project_token();
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .put_forge_token(
            upstream_owner(),
            "tok",
            &ForgeAccount {
                login: "reader-bot".into(),
                id: 1,
            },
        )
        .unwrap();
    let csrf = upstream_csrf(&dashboard).await;
    let salsa = ForgeTokenOwner::Upstream {
        project: "maestro",
        forge: &ForgeKind::Salsa,
    };

    // The form still says GitHub, with packaging off, and names Salsa.
    let cleared = dashboard
        .post(
            &format!("{PROJECT_PAGE}/upstream-token/clear?forge=salsa"),
            &format!("upstream_forge=github&csrf={csrf}"),
        )
        .await;
    assert!(cleared.body.contains("Cleared."));
    assert!(dashboard.store.forge_account(salsa).unwrap().is_none());
    assert!(
        dashboard
            .store
            .forge_account(upstream_owner())
            .unwrap()
            .is_some()
    );

    for forge in ["gitlab", ""] {
        let refused = dashboard
            .post(
                &format!("{PROJECT_PAGE}/upstream-token/clear?forge={forge}"),
                &format!("csrf={csrf}"),
            )
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    }
    assert!(
        dashboard
            .store
            .forge_account(upstream_owner())
            .unwrap()
            .is_some()
    );
}

/// Answers every request with an empty repository as a public API would.
async fn stub_api() -> (String, ()) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let _ = socket.read(&mut buf).await;
                let body = "{}";
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    (base, ())
}

/// A Forgejo stub over TLS that knows one token, `tok-ok`, which belongs to
/// `<prefix>-bot`, and answers 401 to any other. It counts each request, and
/// with a gate it signals `arrived` and then waits for `release` before it
/// answers.
async fn forgejo_gated_stub(
    prefix: &'static str,
    hits: Arc<std::sync::atomic::AtomicUsize>,
    gate: Option<(Arc<Notify>, Arc<Notify>)>,
) -> support::TlsServer {
    let user = axum::routing::get(move |headers: axum::http::HeaderMap| async move {
        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some((arrived, release)) = gate {
            arrived.notify_one();
            release.notified().await;
        }
        let sent = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if sent == Some("token tok-ok") {
            (
                StatusCode::OK,
                json!({"login": format!("{prefix}-bot"), "id": 5}).to_string(),
            )
        } else {
            (StatusCode::UNAUTHORIZED, "{}".to_owned())
        }
    });
    let unread = axum::routing::get(|| async { StatusCode::INTERNAL_SERVER_ERROR });
    support::serve_tls(Router::new().route("/api/v1/user", user).fallback(unread)).await
}

async fn forgejo_user_stub(prefix: &'static str) -> support::TlsServer {
    forgejo_gated_stub(prefix, Arc::default(), None).await
}

fn forgejo_roots(stubs: &[&support::TlsServer]) -> Vec<reqwest::Certificate> {
    stubs
        .iter()
        .map(|stub| reqwest::Certificate::from_pem(stub.ca_pem.as_bytes()).unwrap())
        .collect()
}

async fn forgejo_dashboard() -> Dashboard {
    let mine = forgejo_user_stub("mine").await;
    let other = forgejo_user_stub("other").await;
    forgejo_dashboard_over(&mine, &other)
}

/// As [`forgejo_dashboard`] over these stubs.
fn forgejo_dashboard_over(mine: &support::TlsServer, other: &support::TlsServer) -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .add_forgejo_instance("mine", &mine.base, "17.0.2")
        .unwrap();
    store
        .add_forgejo_instance("other", &other.base, "17.0.2")
        .unwrap();
    let kind = store.resolve_forge("mine").unwrap().unwrap();
    store
        .create_project("forged", &kind, "team/forged", "main", None, Some("alice"))
        .unwrap();
    store
        .create_project(
            "maestro",
            &ForgeKind::GitHub,
            "Badger-Embedded/maestro",
            "main",
            None,
            Some("alice"),
        )
        .unwrap();
    let secrets = Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    let stubs = [mine, other];
    dashboard_build(
        store,
        Arc::new(FakeRepos::fit()),
        None,
        |_| None,
        secrets,
        vec![USER.to_owned()],
        |_| GitHub::default(),
        forgejo_roots(&stubs),
    )
}

/// The csrf token and the server URL the token page of `instance` carries.
async fn forgejo_form_fields(
    dashboard: &Dashboard,
    login: &str,
    instance: &str,
) -> (String, String) {
    let page = dashboard
        .get_as(login, &format!("/settings/forges/{instance}"))
        .await;
    let doc = scraper::Html::parse_document(&page.body);
    let field = |name: &str| {
        doc.select(&scraper::Selector::parse(&format!("input[name={name}]")).unwrap())
            .next()
            .and_then(|input| input.value().attr("value"))
            .unwrap_or_else(|| panic!("the form carries {name}"))
            .to_owned()
    };
    (field("csrf"), field("server"))
}

async fn forgejo_post(
    dashboard: &Dashboard,
    login: &str,
    instance: &str,
    action: &str,
    form: &str,
) -> Reply {
    let (csrf, server) = forgejo_form_fields(dashboard, login, instance).await;
    forgejo_post_with(dashboard, login, instance, action, form, &csrf, &server).await
}

async fn forgejo_post_with(
    dashboard: &Dashboard,
    login: &str,
    instance: &str,
    action: &str,
    form: &str,
    csrf: &str,
    server: &str,
) -> Reply {
    dashboard
        .post_as(
            login,
            &format!("/settings/forges/{instance}/{action}"),
            &format!("{form}&csrf={csrf}&server={server}"),
        )
        .await
}

#[tokio::test]
async fn a_forgejo_instance_has_a_token_page_that_names_the_scopes_and_the_sidebar_lists_it() {
    let dashboard = forgejo_dashboard().await;
    let page = dashboard.get_as(DAVE, "/settings/forges/mine").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No token is stored."));
    for scope in ["read:user", "write:repository", "write:issue"] {
        assert!(page.body.contains(scope), "{scope}");
    }
    assert!(page.body.contains("href=\"/settings/forges/other\""));

    let github = dashboard.get_as(DAVE, "/settings/forges/github").await;
    assert!(!github.body.contains("write:repository"));
}

#[tokio::test]
async fn a_forgejo_token_is_checked_against_its_instance_and_stored_with_its_login() {
    let dashboard = forgejo_dashboard().await;
    let mine: &'static ForgeKind = Box::leak(Box::new(
        dashboard.store.resolve_forge("mine").unwrap().unwrap(),
    ));
    let owner = || ForgeTokenOwner::User {
        login: DAVE,
        forge: mine,
    };

    let rejected = forgejo_post(&dashboard, DAVE, "mine", "token", "token=tok-bad").await;
    assert!(rejected.body.contains("mine rejected the token"));
    assert!(!rejected.body.contains("tok-bad"));
    assert!(dashboard.store.forge_account(owner()).unwrap().is_none());

    let saved = forgejo_post(&dashboard, DAVE, "mine", "token", "token=tok-ok").await;
    assert!(saved.body.contains("mine-bot"));
    assert!(!saved.body.contains("tok-ok"));
    let account = dashboard.store.forge_account(owner()).unwrap().unwrap();
    assert_eq!(account.login, "mine-bot");

    forgejo_post(&dashboard, DAVE, "mine", "token", "token=").await;
    assert!(dashboard.store.forge_account(owner()).unwrap().is_some());

    let cleared = forgejo_post(&dashboard, DAVE, "mine", "token/clear", "").await;
    assert!(cleared.body.contains("No token is stored."));
    assert!(dashboard.store.forge_account(owner()).unwrap().is_none());
}

#[tokio::test]
async fn two_forgejo_instances_keep_their_own_token_of_one_login() {
    let dashboard = forgejo_dashboard().await;
    forgejo_post(&dashboard, DAVE, "mine", "token", "token=tok-ok").await;
    forgejo_post(&dashboard, DAVE, "other", "token", "token=tok-ok").await;

    let mine = dashboard.get_as(DAVE, "/settings/forges/mine").await.body;
    let other = dashboard.get_as(DAVE, "/settings/forges/other").await.body;
    assert!(mine.contains("mine-bot") && !mine.contains("other-bot"));
    assert!(other.contains("other-bot") && !other.contains("mine-bot"));

    forgejo_post(&dashboard, DAVE, "mine", "token/clear", "").await;
    let other = dashboard.get_as(DAVE, "/settings/forges/other").await.body;
    assert!(other.contains("other-bot"));
}

#[tokio::test]
async fn a_deleted_and_re_added_instance_has_no_token() {
    let dashboard = forgejo_dashboard().await;
    forgejo_post(&dashboard, DAVE, "other", "token", "token=tok-ok").await;
    let url = dashboard
        .store
        .forgejo_instance("other")
        .unwrap()
        .unwrap()
        .base_url;

    dashboard.store.delete_forgejo_instance("other").unwrap();
    assert_eq!(
        dashboard
            .get_as(DAVE, "/settings/forges/other")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    dashboard
        .store
        .add_forgejo_instance("other", &url, "17.0.2")
        .unwrap();
    let page = dashboard.get_as(DAVE, "/settings/forges/other").await.body;
    assert!(page.contains("No token is stored."));
}

#[tokio::test]
async fn an_unregistered_forgejo_name_is_a_404_and_its_project_shows_the_problem() {
    let dashboard = forgejo_dashboard().await;
    assert_eq!(
        dashboard.get_as(DAVE, "/settings/forges/gone").await.status,
        StatusCode::NOT_FOUND
    );
    let gone = ForgeKind::Forgejo(factory_runner::ForgejoServer {
        name: "gone".to_owned(),
        base_url: "https://gone.example.org".to_owned(),
        registration: 1,
    });
    dashboard
        .store
        .create_project("lost", &gone, "team/lost", "main", None, Some("alice"))
        .unwrap();
    dashboard
        .store
        .add_collaborator("lost", USER, "test")
        .unwrap();

    let settings = dashboard.get("/settings/projects/lost").await;
    assert_eq!(settings.status, StatusCode::OK);
    assert!(settings.body.contains("does not support"));
    let board = dashboard.get("/?project=lost").await;
    assert!(board.body.contains("gone"));
    assert!(!board.body.contains("source-link"));
    let healthy = dashboard.get("/?project=forged").await;
    assert_eq!(healthy.status, StatusCode::OK);
    assert!(!healthy.body.contains("does not support"));
}

#[tokio::test]
async fn a_forgejo_project_links_its_repository_on_the_instance_in_a_new_tab() {
    let dashboard = forgejo_dashboard().await;
    let base = dashboard
        .store
        .forgejo_instance("mine")
        .unwrap()
        .unwrap()
        .base_url;
    let board = dashboard.get("/?project=forged").await;
    assert!(board.body.contains(&format!("href=\"{base}/team/forged\"")));
    assert!(support::check_link_targets(&board.body) >= 1);
    let settings = dashboard.get("/settings/projects/forged").await;
    assert!(settings.body.contains(&format!("{base}/team/forged")));
    assert!(settings.body.contains("Forge account"));
}

#[tokio::test]
async fn a_token_form_opened_for_an_instance_registered_again_is_refused_before_any_forge_call() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let hits = Arc::new(AtomicUsize::new(0));
    let mine = forgejo_user_stub("mine").await;
    let other = forgejo_gated_stub("other", Arc::clone(&hits), None).await;
    let dashboard = forgejo_dashboard_over(&mine, &other);
    let (csrf, shown) = forgejo_form_fields(&dashboard, DAVE, "other").await;

    dashboard.store.delete_forgejo_instance("other").unwrap();
    dashboard
        .store
        .add_forgejo_instance("other", &other.base, "17.0.2")
        .unwrap();

    for action in ["token", "token/clear"] {
        let reply = forgejo_post_with(
            &dashboard,
            DAVE,
            "other",
            action,
            "token=tok-ok",
            &csrf,
            &shown,
        )
        .await;
        assert!(
            reply.body.contains("changed since this page loaded"),
            "{action}"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let page = dashboard.get_as(DAVE, "/settings/forges/other").await.body;
    assert!(page.contains("No token is stored."));
}

#[tokio::test]
async fn a_token_check_that_outlives_its_instance_stores_nothing() {
    use std::sync::atomic::AtomicUsize;

    let arrived = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mine = forgejo_user_stub("mine").await;
    let other = forgejo_gated_stub(
        "other",
        Arc::new(AtomicUsize::new(0)),
        Some((Arc::clone(&arrived), Arc::clone(&release))),
    )
    .await;
    let dashboard = forgejo_dashboard_over(&mine, &other);
    let (csrf, shown) = forgejo_form_fields(&dashboard, DAVE, "other").await;

    let save = forgejo_post_with(
        &dashboard,
        DAVE,
        "other",
        "token",
        "token=tok-ok",
        &csrf,
        &shown,
    );
    let replace = async {
        arrived.notified().await;
        dashboard.store.delete_forgejo_instance("other").unwrap();
        dashboard
            .store
            .add_forgejo_instance("other", &other.base, "17.0.2")
            .unwrap();
        release.notify_one();
    };
    let (reply, ()) = tokio::join!(save, replace);

    assert!(reply.body.contains("deleted or registered again"));
    assert!(!reply.body.contains("other-bot"));
    let page = dashboard.get_as(DAVE, "/settings/forges/other").await.body;
    assert!(page.contains("No token is stored."));
}

async fn maintenance_csrf(dashboard: &Dashboard) -> String {
    let page = dashboard.get("/settings/projects/maestro").await;
    let doc = scraper::Html::parse_document(&page.body);
    doc.select(
        &scraper::Selector::parse("form[action$=\"/maintenance\"] input[name=csrf]").unwrap(),
    )
    .next()
    .and_then(|input| input.value().attr("value"))
    .expect("the maintenance form carries a csrf token")
    .to_owned()
}

#[tokio::test]
async fn an_upstream_token_form_opened_for_an_instance_registered_again_is_refused() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let hits = Arc::new(AtomicUsize::new(0));
    let mine = forgejo_user_stub("mine").await;
    let other = forgejo_gated_stub("other", Arc::clone(&hits), None).await;
    let dashboard = forgejo_dashboard_over(&mine, &other);
    dashboard
        .store
        .add_collaborator("maestro", USER, "test")
        .unwrap();
    let shown = dashboard
        .store
        .forgejo_instance("other")
        .unwrap()
        .unwrap()
        .registration;
    dashboard.store.delete_forgejo_instance("other").unwrap();
    dashboard
        .store
        .add_forgejo_instance("other", &other.base, "17.0.2")
        .unwrap();

    let csrf = maintenance_csrf(&dashboard).await;
    let form = format!(
        "renovate=on&packaging=on&packaging_type=deb&manifest_files=debian%2Fcontrol\
         &upstream_repo=o%2Ftool&upstream_forge=other&server_other={shown}&token_other=tok-ok&csrf={csrf}"
    );
    for action in ["", "/clear"] {
        let reply = dashboard
            .post(
                &format!("/settings/projects/maestro/upstream-token{action}?forge=other"),
                &form,
            )
            .await;
        assert!(
            reply.body.contains("changed since this page loaded"),
            "{action}"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_unregistered_upstream_forge_is_refused_before_any_forge_call() {
    let dashboard = forgejo_dashboard().await;
    dashboard
        .store
        .add_collaborator("maestro", USER, "test")
        .unwrap();
    let form = "renovate=on&packaging=on&packaging_type=deb&manifest_files=debian%2Fcontrol\
                &upstream_repo=o%2Ftool&upstream_forge=nope";

    let check = dashboard
        .post("/settings/projects/maestro/maintenance/check", form)
        .await;
    assert!(
        check
            .body
            .contains("is not registered. Pick another upstream forge.")
    );
    assert!(!check.body.contains("Checked with"));

    let save = dashboard
        .post("/settings/projects/maestro/maintenance", form)
        .await;
    assert!(
        save.body
            .contains("is not registered. Pick another upstream forge.")
    );
    assert!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .is_none()
    );
}

#[tokio::test]
async fn a_project_whose_stored_upstream_forge_is_gone_stays_editable() {
    let dashboard = forgejo_dashboard().await;
    dashboard
        .store
        .add_collaborator("maestro", USER, "test")
        .unwrap();
    let gone = ForgeKind::Forgejo(factory_runner::ForgejoServer {
        name: "gone".to_owned(),
        base_url: "https://gone.example.org".to_owned(),
        registration: 1,
    });
    dashboard
        .store
        .save_project_maintenance(
            "maestro",
            &factory_server::Maintenance {
                renovate: true,
                renovate_author: None,
                packaging: Some(factory_server::Packaging {
                    kind: factory_server::PackagingType::Deb,
                    manifest_files: vec!["debian/control".to_owned()],
                    build_config_files: Vec::new(),
                    upstream_repo: "o/tool".to_owned(),
                    upstream_forge: gone,
                }),
            },
        )
        .unwrap();

    let page = dashboard.get("/settings/projects/maestro").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains("is no longer registered, so the upstream is shown as GitHub")
    );
    let saved = dashboard
        .post("/settings/projects/maestro/maintenance", "x=1")
        .await;
    assert_eq!(saved.status, StatusCode::OK);
    assert!(
        dashboard
            .store
            .project_maintenance("maestro")
            .unwrap()
            .packaging
            .is_none()
    );
    assert!(!saved.body.contains("is no longer registered, so"));
}

#[tokio::test]
async fn repairing_a_gone_upstream_keeps_the_github_upstream_token() {
    let dashboard = dashboard_with_project_token();
    let upstream = ForgeTokenOwner::Upstream {
        project: "maestro",
        forge: &ForgeKind::GitHub,
    };
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .put_forge_token(
            upstream,
            "tok-github",
            &ForgeAccount {
                login: "gh-bot".into(),
                id: 3,
            },
        )
        .unwrap();
    let gone = ForgeKind::Forgejo(factory_runner::ForgejoServer {
        name: "gone".to_owned(),
        base_url: "https://gone.example.org".to_owned(),
        registration: 1,
    });
    dashboard
        .store
        .save_project_maintenance(
            "maestro",
            &factory_server::Maintenance {
                renovate: true,
                renovate_author: None,
                packaging: Some(factory_server::Packaging {
                    kind: factory_server::PackagingType::Deb,
                    manifest_files: vec!["debian/control".to_owned()],
                    build_config_files: Vec::new(),
                    upstream_repo: "o/tool".to_owned(),
                    upstream_forge: gone,
                }),
            },
        )
        .unwrap();
    let saved = dashboard
        .post(
            "/settings/projects/maestro/maintenance",
            "renovate=on&packaging=on&packaging_type=deb&manifest_files=debian%2Fcontrol\
             &upstream_repo=team%2Ftool&upstream_forge=salsa",
        )
        .await;

    let kept = dashboard.store.project_maintenance("maestro").unwrap();
    assert_eq!(
        kept.packaging.map(|p| p.upstream_forge.as_str().to_owned()),
        Some("salsa".to_owned()),
        "{}",
        saved.body
    );
    assert!(dashboard.store.forge_account(upstream).unwrap().is_some());
}

/// What the onboarding stub of the Forgejo instance `mine` answers for the
/// repository `team/proj`. A `None` field is answered with 404.
#[derive(Clone)]
struct ForgejoCase {
    repo: Option<serde_json::Value>,
    branch: Option<serde_json::Value>,
    forgejo_workflows: Option<serde_json::Value>,
    github_workflows: Option<serde_json::Value>,
}

impl ForgejoCase {
    fn fit() -> ForgejoCase {
        ForgejoCase {
            repo: Some(json!({
                "permissions": {"admin": false, "push": true, "pull": true},
                "allow_squash_merge": true,
                "has_pull_requests": true,
                "has_actions": true,
            })),
            branch: Some(json!({"protected": true, "user_can_merge": true})),
            forgejo_workflows: Some(json!([{"name": "ci.yaml", "type": "file"}])),
            github_workflows: None,
        }
    }

    fn repo_with(mut self, field: &str, value: serde_json::Value) -> ForgejoCase {
        self.repo.as_mut().unwrap()[field] = value;
        self
    }
}

type Gate = Arc<std::sync::Mutex<Option<(Arc<Notify>, Arc<Notify>)>>>;

struct ForgejoOnboarding {
    dashboard: Dashboard,
    case: Arc<std::sync::Mutex<ForgejoCase>>,
    hits: Arc<std::sync::atomic::AtomicUsize>,
    /// While set, the stub signals the first and then waits for the second
    /// before it answers a repository read.
    gate: Gate,
}

async fn forgejo_onboarding() -> ForgejoOnboarding {
    let case = Arc::new(std::sync::Mutex::new(ForgejoCase::fit()));
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = Gate::default();
    let handler = {
        let (case, hits, gate) = (Arc::clone(&case), Arc::clone(&hits), Arc::clone(&gate));
        move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
            let (case, hits, gate) = (Arc::clone(&case), Arc::clone(&hits), Arc::clone(&gate));
            async move {
                let held = if uri.path() == "/api/v1/repos/team/proj" {
                    gate.lock().unwrap().clone()
                } else {
                    None
                };
                if let Some((arrived, release)) = held {
                    arrived.notify_one();
                    release.notified().await;
                }
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let sent = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok());
                if sent != Some("token tok-ok") {
                    return (StatusCode::UNAUTHORIZED, "{}".to_owned());
                }
                let case = case.lock().unwrap().clone();
                let answer = match uri.path() {
                    "/api/v1/user" => Some(json!({"login": "mine-bot", "id": 5})),
                    "/api/v1/repos/team/proj" => case.repo,
                    "/api/v1/repos/team/proj/branches/main" => case.branch,
                    "/api/v1/repos/team/proj/contents/.forgejo/workflows" => case.forgejo_workflows,
                    "/api/v1/repos/team/proj/contents/.github/workflows" => case.github_workflows,
                    _ => None,
                };
                match answer {
                    Some(answer) => (StatusCode::OK, answer.to_string()),
                    None => (StatusCode::NOT_FOUND, "{}".to_owned()),
                }
            }
        }
    };
    let stub = support::serve_tls(Router::new().fallback(handler)).await;
    let other = forgejo_user_stub("other").await;
    let dashboard = forgejo_dashboard_over(&stub, &other);
    ForgejoOnboarding {
        dashboard,
        case,
        hits,
        gate,
    }
}

impl ForgejoOnboarding {
    /// The form fields that carry the registration of `mine`, as its page does.
    fn shown(&self) -> String {
        let Some(ForgeKind::Forgejo(server)) = self.dashboard.store.resolve_forge("mine").unwrap()
        else {
            panic!("mine is registered");
        };
        format!("server_mine={}", server.registration)
    }

    async fn store_token(&self) {
        forgejo_post(&self.dashboard, USER, "mine", "token", "token=tok-ok").await;
    }

    async fn check(&self, case: ForgejoCase, repo: &str) -> String {
        *self.case.lock().unwrap() = case;
        self.dashboard
            .post(
                "/onboard",
                &format!(
                    "forge=mine&{}&repo={repo}&name=proj&action=check",
                    self.shown()
                ),
            )
            .await
            .body
    }
}

#[tokio::test]
async fn onboarding_on_a_forgejo_instance_blocks_on_each_failed_check() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let cases = [
        (ForgejoCase::fit(), "team", "is not in owner/name form"),
        (
            ForgejoCase {
                repo: None,
                ..ForgejoCase::fit()
            },
            "team%2Fproj",
            "does not exist or is not visible to mine-bot",
        ),
        (
            ForgejoCase::fit().repo_with("permissions", json!({"push": false, "pull": true})),
            "team%2Fproj",
            "mine-bot has no push access to team/proj",
        ),
        (
            ForgejoCase {
                branch: None,
                ..ForgejoCase::fit()
            },
            "team%2Fproj",
            "The base branch &quot;main&quot; does not exist",
        ),
        (
            ForgejoCase::fit().repo_with("allow_squash_merge", json!(false)),
            "team%2Fproj",
            "does not allow squash merges",
        ),
        (
            ForgejoCase::fit().repo_with("has_pull_requests", json!(false)),
            "team%2Fproj",
            "has pull requests turned off",
        ),
    ];
    for (case, repo, expected) in cases {
        let body = onboarding.check(case, repo).await;
        assert!(body.contains(expected), "{expected}: {body}");
        assert!(!body.contains("Skip extras and create"), "{expected}");
    }
}

#[tokio::test]
async fn onboarding_on_a_forgejo_instance_warns_without_blocking() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let cases = [
        (
            ForgejoCase::fit().repo_with(
                "permissions",
                json!({"admin": true, "push": true, "pull": true}),
            ),
            "mine-bot has Admin access",
        ),
        (
            ForgejoCase {
                branch: Some(json!({"protected": false, "user_can_merge": true})),
                ..ForgejoCase::fit()
            },
            "has no protection",
        ),
        (
            ForgejoCase {
                branch: Some(json!({"protected": true, "user_can_merge": false})),
                ..ForgejoCase::fit()
            },
            "does not let mine-bot merge",
        ),
        (
            ForgejoCase::fit().repo_with("has_actions", json!(false)),
            "has no CI workflow",
        ),
        (
            ForgejoCase {
                forgejo_workflows: None,
                ..ForgejoCase::fit()
            },
            "has no CI workflow",
        ),
        (
            ForgejoCase {
                forgejo_workflows: Some(json!([{"name": "README.md", "type": "file"}])),
                ..ForgejoCase::fit()
            },
            "has no CI workflow",
        ),
    ];
    for (case, expected) in cases {
        let body = onboarding.check(case, "team%2Fproj").await;
        assert!(body.contains(expected), "{expected}: {body}");
        assert!(body.contains("All required checks passed."), "{expected}");
        assert!(body.contains("Skip extras and create"), "{expected}");
    }

    let github_only = ForgejoCase {
        forgejo_workflows: None,
        github_workflows: Some(json!([{"name": "ci.yml", "type": "file"}])),
        ..ForgejoCase::fit()
    };
    let body = onboarding.check(github_only, "team%2Fproj").await;
    assert!(!body.contains("check-warnings"), "{body}");
}

#[tokio::test]
async fn a_fit_forgejo_repository_onboards_once_under_its_instance() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let form = format!(
        "forge=mine&{}&repo=team%2Fproj&action=skip",
        onboarding.shown()
    );
    let reply = onboarding.dashboard.post("/onboard", &form).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let project = onboarding.dashboard.store.project("proj").unwrap().unwrap();
    let ProjectForge::Known(kind) = project.forge else {
        panic!("the forge is known");
    };
    assert_eq!(kind.as_str(), "mine");

    let again = onboarding
        .dashboard
        .post(
            "/onboard",
            &format!(
                "forge=mine&{}&repo=team%2Fproj&name=again&action=skip",
                onboarding.shown()
            ),
        )
        .await;
    assert!(
        again
            .body
            .contains("The mine repository team/proj is already onboarded.")
    );
}

#[tokio::test]
async fn forgejo_onboarding_without_a_token_links_the_token_page_and_calls_nothing() {
    let onboarding = forgejo_onboarding().await;
    let before = onboarding.hits.load(std::sync::atomic::Ordering::SeqCst);
    let body = onboarding.check(ForgejoCase::fit(), "team%2Fproj").await;
    assert!(body.contains("You have no mine token stored"));
    assert!(body.contains("href=\"/settings/forges/mine\""));
    assert_eq!(
        onboarding.hits.load(std::sync::atomic::Ordering::SeqCst),
        before
    );
}

#[tokio::test]
async fn the_onboarding_page_shows_the_hint_of_the_selected_forge_without_script() {
    let onboarding = forgejo_onboarding().await;
    let body = onboarding.dashboard.get("/onboard").await.body;
    assert!(body.contains("value=\"mine\""));
    let doc = scraper::Html::parse_document(&body);
    let visible: Vec<_> = doc
        .select(&scraper::Selector::parse("p.hint[data-forge]:not([hidden])").unwrap())
        .map(|hint| hint.value().attr("data-forge").unwrap().to_owned())
        .collect();
    assert_eq!(visible, ["github"]);
    let mine = doc
        .select(&scraper::Selector::parse("p.hint[data-forge=mine]").unwrap())
        .count();
    assert_eq!(mine, 1);

    let posted = onboarding
        .dashboard
        .post(
            "/onboard",
            &format!("forge=mine&{}&repo=&action=check", onboarding.shown()),
        )
        .await
        .body;
    let doc = scraper::Html::parse_document(&posted);
    let visible: Vec<_> = doc
        .select(&scraper::Selector::parse("p.hint[data-forge]:not([hidden])").unwrap())
        .map(|hint| hint.value().attr("data-forge").unwrap().to_owned())
        .collect();
    assert_eq!(visible, ["mine"]);
}

#[tokio::test]
async fn forgejo_onboarding_rejects_dot_segments_before_any_forge_call() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let before = onboarding.hits.load(std::sync::atomic::Ordering::SeqCst);
    for repo in ["team%2F..", "..%2Fproj", "team%2F.", ".%2Fproj"] {
        let body = onboarding.check(ForgejoCase::fit(), repo).await;
        assert!(body.contains("is not in owner/name form"), "{repo}: {body}");
    }
    assert_eq!(
        onboarding.hits.load(std::sync::atomic::Ordering::SeqCst),
        before
    );
}

#[tokio::test]
async fn a_repository_with_a_workflow_path_that_is_a_file_or_missing_warns_without_blocking() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let cases = [
        ForgejoCase {
            forgejo_workflows: Some(json!({"name": ".forgejo/workflows", "type": "file"})),
            ..ForgejoCase::fit()
        },
        ForgejoCase {
            forgejo_workflows: None,
            github_workflows: Some(json!({"name": "workflows", "type": "symlink"})),
            ..ForgejoCase::fit()
        },
    ];
    for case in cases {
        let body = onboarding.check(case, "team%2Fproj").await;
        assert!(body.contains("has no CI workflow"), "{body}");
        assert!(body.contains("All required checks passed."), "{body}");
        assert!(!body.contains("could not run"), "{body}");
    }

    let file_then_directory = ForgejoCase {
        forgejo_workflows: Some(json!({"name": ".forgejo/workflows", "type": "file"})),
        github_workflows: Some(json!([{"name": "ci.yml", "type": "file"}])),
        ..ForgejoCase::fit()
    };
    let body = onboarding.check(file_then_directory, "team%2Fproj").await;
    assert!(!body.contains("has no CI workflow"), "{body}");
}

#[tokio::test]
async fn an_unexpected_workflow_listing_is_a_check_error() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let malformed = [
        json!(null),
        json!(7),
        json!("file"),
        json!({"name": "workflows"}),
        json!({"type": "file"}),
        json!({"name": "workflows", "type": "dir"}),
        json!([{"name": 3, "type": "file"}]),
        json!([{}]),
        json!([{"name": "ci.yml"}]),
        json!([{"type": "file"}]),
    ];
    for listing in malformed {
        let case = ForgejoCase {
            forgejo_workflows: Some(listing.clone()),
            ..ForgejoCase::fit()
        };
        let body = onboarding.check(case, "team%2Fproj").await;
        assert!(
            body.contains("returned unexpected output"),
            "{listing}: {body}"
        );
        assert!(!body.contains("All required checks passed."), "{listing}");
    }
}

#[tokio::test]
async fn a_stale_forgejo_onboarding_form_runs_no_check_and_creates_nothing() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let before = onboarding.hits.load(std::sync::atomic::Ordering::SeqCst);
    for fields in ["", "&server_mine=999999"] {
        let body = onboarding
            .dashboard
            .post(
                "/onboard",
                &format!("forge=mine{fields}&repo=team%2Fproj&action=skip"),
            )
            .await
            .body;
        assert!(body.contains("changed since this page loaded"), "{body}");
        assert_no_forge_selected(&body);
    }
    assert_eq!(
        onboarding.hits.load(std::sync::atomic::Ordering::SeqCst),
        before
    );
    assert!(
        onboarding
            .dashboard
            .store
            .project("proj")
            .unwrap()
            .is_none()
    );
}

fn assert_no_forge_selected(body: &str) {
    let radios: Vec<&str> = body
        .split("<input type=\"radio\" name=\"forge\"")
        .skip(1)
        .map(|rest| rest.split('>').next().unwrap())
        .collect();
    assert!(radios.len() >= 3, "{body}");
    for radio in radios {
        assert!(!radio.contains("checked"), "{radio}");
        assert!(radio.contains("required"), "{radio}");
    }
    assert!(body.contains("name=\"choose_forge\""), "{body}");
}

#[tokio::test]
async fn an_empty_forge_after_a_stale_redraw_is_refused_not_read_as_github() {
    let onboarding = forgejo_onboarding().await;
    onboarding.store_token().await;
    let before = onboarding.hits.load(std::sync::atomic::Ordering::SeqCst);
    let body = onboarding
        .dashboard
        .post(
            "/onboard",
            "forge=&choose_forge=1&repo=team%2Fproj&name=proj&action=skip",
        )
        .await
        .body;
    assert!(body.contains("Choose a forge."), "{body}");
    assert_no_forge_selected(&body);
    assert_eq!(
        onboarding.hits.load(std::sync::atomic::Ordering::SeqCst),
        before
    );
}

#[tokio::test]
async fn an_instance_changed_while_a_submit_waits_for_the_token_read_is_a_wizard_error() {
    let onboarding = forgejo_onboarding().await;
    onboarding.dashboard.store.remove_project("forged").unwrap();
    onboarding.store_token().await;
    let url = onboarding
        .dashboard
        .store
        .forgejo_instance("mine")
        .unwrap()
        .unwrap()
        .base_url;
    for replace in [false, true] {
        let arrived = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *onboarding.gate.lock().unwrap() = Some((Arc::clone(&arrived), Arc::clone(&release)));
        let form = format!(
            "forge=mine&{}&repo=team%2Fproj&name=proj&action=check",
            onboarding.shown()
        );
        let holder = onboarding.dashboard.post("/onboard", &form);
        let waiter = async {
            arrived.notified().await;
            // The second submit passes the form check, then waits for the
            // onboarding lock the first holds.
            let second = onboarding.dashboard.post("/onboard", &form);
            let change = async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let store = &onboarding.dashboard.store;
                store.delete_forgejo_instance("mine").unwrap();
                if replace {
                    store.add_forgejo_instance("mine", &url, "17.0.2").unwrap();
                }
                release.notify_one();
            };
            tokio::join!(second, change).0
        };
        let (_, second) = tokio::join!(holder, waiter);
        *onboarding.gate.lock().unwrap() = None;
        assert_eq!(second.status, StatusCode::OK);
        assert!(
            second.body.contains("deleted or registered again"),
            "{}",
            second.body
        );
        assert_no_forge_selected(&second.body);
        if !replace {
            onboarding
                .dashboard
                .store
                .add_forgejo_instance("mine", &url, "17.0.2")
                .unwrap();
            onboarding.store_token().await;
        }
    }
}

#[tokio::test]
async fn an_instance_deleted_or_replaced_during_the_checks_creates_no_project() {
    let onboarding = forgejo_onboarding().await;
    onboarding.dashboard.store.remove_project("forged").unwrap();
    onboarding.store_token().await;
    let url = onboarding
        .dashboard
        .store
        .forgejo_instance("mine")
        .unwrap()
        .unwrap()
        .base_url;
    for (replace, action) in [
        (false, "skip"),
        (true, "skip"),
        (false, "check"),
        (true, "check"),
    ] {
        let arrived = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *onboarding.gate.lock().unwrap() = Some((Arc::clone(&arrived), Arc::clone(&release)));
        let form = format!(
            "forge=mine&{}&repo=team%2Fproj&action={action}",
            onboarding.shown()
        );
        let submit = onboarding.dashboard.post("/onboard", &form);
        let change = async {
            arrived.notified().await;
            onboarding
                .dashboard
                .store
                .delete_forgejo_instance("mine")
                .unwrap();
            if replace {
                onboarding
                    .dashboard
                    .store
                    .add_forgejo_instance("mine", &url, "17.0.2")
                    .unwrap();
            }
            release.notify_one();
        };
        let (reply, ()) = tokio::join!(submit, change);
        *onboarding.gate.lock().unwrap() = None;
        assert!(
            reply.body.contains("deleted or registered again"),
            "{}",
            reply.body
        );
        assert!(!reply.body.contains("All required checks passed."));
        assert_no_forge_selected(&reply.body);
        assert!(
            onboarding
                .dashboard
                .store
                .project("proj")
                .unwrap()
                .is_none()
        );
        if !replace {
            onboarding
                .dashboard
                .store
                .add_forgejo_instance("mine", &url, "17.0.2")
                .unwrap();
        }
        onboarding.store_token().await;
    }
}

/// A Forgejo stub over TLS that serves `repos` as `(owner/name, push)` and
/// answers 404 to every other repository, 403 to `denied` and 502 to `flaky`.
async fn forgejo_repo_stub(repos: &'static [(&'static str, bool)]) -> support::TlsServer {
    let repo = axum::routing::get(
        move |axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>| async move {
            let path = format!("{owner}/{name}");
            if name == "denied" {
                return (StatusCode::FORBIDDEN, "{}".to_owned());
            }
            if name == "flaky" {
                return (StatusCode::BAD_GATEWAY, "HTTP 403".to_owned());
            }
            match repos.iter().find(|(known, _)| *known == path) {
                Some((_, push)) => (
                    StatusCode::OK,
                    json!({"permissions": {"admin": false, "push": push, "pull": true}})
                        .to_string(),
                ),
                None => (StatusCode::NOT_FOUND, "{}".to_owned()),
            }
        },
    );
    support::serve_tls(Router::new().route("/api/v1/repos/{owner}/{name}", repo)).await
}

/// A dashboard with `forged` on instance `mine`, which holds the project
/// token Renovate needs, and an instance `other`.
async fn forgejo_maintenance_dashboard() -> Dashboard {
    static MINE: &[(&str, bool)] = &[("team/forged", true), ("team/tool", true)];
    static OTHER: &[(&str, bool)] = &[("o/tool", false)];
    let mine = forgejo_repo_stub(MINE).await;
    let other = forgejo_repo_stub(OTHER).await;
    let dashboard = forgejo_dashboard_over(&mine, &other);
    dashboard
        .store
        .add_collaborator("forged", USER, "test")
        .unwrap();
    let kind = dashboard.store.resolve_forge("mine").unwrap().unwrap();
    LocalSecrets::new(&[7; MASTER_KEY_LEN], dashboard.store.clone())
        .put_forge_token(
            ForgeTokenOwner::Project {
                project: "forged",
                forge: &kind,
            },
            "tok-mine",
            &ForgeAccount {
                login: "mine-bot".into(),
                id: 5,
            },
        )
        .unwrap();
    dashboard
}

/// The `server_<name>` fields the Maintenance card posts for its instances.
fn forgejo_server_fields(dashboard: &Dashboard) -> String {
    let mut fields = String::new();
    for name in ["mine", "other"] {
        let server = dashboard.store.forgejo_instance(name).unwrap().unwrap();
        write!(fields, "&server_{name}={}", server.registration).unwrap();
    }
    fields
}

#[tokio::test]
async fn a_forgejo_project_needs_its_renovate_account_like_a_salsa_one() {
    let dashboard = forgejo_maintenance_dashboard().await;
    let page = dashboard.get("/settings/projects/forged").await.body;
    assert!(page.contains("name=\"renovate_author\""));

    let reply = dashboard
        .post("/settings/projects/forged/maintenance", "renovate=on")
        .await;
    assert!(reply.body.contains("Give the Renovate account"));
    assert!(reply.body.contains("the mine username"));
    let reply = dashboard
        .post(
            "/settings/projects/forged/maintenance",
            "renovate=on&renovate_author=a%2Fb",
        )
        .await;
    assert!(reply.body.contains("must be a mine username"));
    assert!(
        !dashboard
            .store
            .project_maintenance("forged")
            .unwrap()
            .renovate
    );

    let reply = dashboard
        .post(
            "/settings/projects/forged/maintenance",
            "renovate=on&renovate_author=+renovate-bot+",
        )
        .await;
    assert!(reply.body.contains("Saved."), "{}", reply.body);
    let saved = dashboard.store.project_maintenance("forged").unwrap();
    assert!(saved.renovate);
    assert_eq!(saved.renovate_author.as_deref(), Some("renovate-bot"));
}

#[tokio::test]
async fn an_upstream_on_a_forgejo_instance_saves_when_the_check_reads_it() {
    let dashboard = forgejo_maintenance_dashboard().await;
    let servers = forgejo_server_fields(&dashboard);
    let form = |repo: &str, forge: &str| {
        format!(
            "renovate=on&renovate_author=renovate-bot&packaging=on&packaging_type=deb\
             &manifest_files=debian%2Fcontrol&upstream_repo={repo}&upstream_forge={forge}{servers}"
        )
    };

    let url = "/settings/projects/forged/maintenance";
    let checked = dashboard
        .post(&format!("{url}/check"), &form("o%2Ftool", "other"))
        .await;
    assert!(
        checked
            .body
            .contains("Checked with no token: o/tool is readable on other."),
        "{}",
        checked.body
    );
    let refused = dashboard.post(url, &form("o%2Fprivate", "other")).await;
    assert!(
        refused
            .body
            .contains("o/private could not be read on other. Save an upstream token")
    );
    assert!(
        dashboard
            .store
            .project_maintenance("forged")
            .unwrap()
            .packaging
            .is_none()
    );

    let own = dashboard
        .post(&format!("{url}/check"), &form("team%2Ftool", "mine"))
        .await;
    assert!(
        own.body
            .contains("Checked with the project token of mine-bot: team/tool is readable on mine."),
        "{}",
        own.body
    );

    let saved = dashboard.post(url, &form("o%2Ftool", "other")).await;
    assert!(saved.body.contains("Saved."), "{}", saved.body);
    let packaging = dashboard
        .store
        .project_maintenance("forged")
        .unwrap()
        .packaging
        .unwrap();
    assert_eq!(packaging.upstream_forge.as_str(), "other");
    assert_eq!(packaging.upstream_repo, "o/tool");

    let page = dashboard.get("/settings/projects/forged").await.body;
    for name in ["mine", "other"] {
        assert!(page.contains(&format!("name=\"token_{name}\"")), "{name}");
        assert!(
            page.contains(&format!("upstream-token?forge={name}\"")),
            "{name}"
        );
    }
    assert!(page.contains("<option value=\"other\" selected"));
}

#[tokio::test]
async fn a_forgejo_upstream_that_answers_403_refuses_the_save_and_stores_nothing() {
    let dashboard = forgejo_maintenance_dashboard().await;
    let servers = forgejo_server_fields(&dashboard);
    let form = format!(
        "renovate=on&renovate_author=renovate-bot&packaging=on&packaging_type=deb\
         &manifest_files=debian%2Fcontrol&upstream_repo=o%2Fdenied&upstream_forge=other{servers}"
    );
    let url = "/settings/projects/forged/maintenance";
    let checked = dashboard.post(&format!("{url}/check"), &form).await;
    assert!(
        checked
            .body
            .contains("o/denied could not be read on other. Save an upstream token"),
        "{}",
        checked.body
    );
    let refused = dashboard.post(url, &form).await;
    assert!(
        refused.body.contains("Nothing was saved."),
        "{}",
        refused.body
    );
    assert!(
        dashboard
            .store
            .project_maintenance("forged")
            .unwrap()
            .packaging
            .is_none()
    );
}

#[tokio::test]
async fn a_forgejo_upstream_that_answers_502_still_saves_and_says_it_was_not_checked() {
    let dashboard = forgejo_maintenance_dashboard().await;
    let servers = forgejo_server_fields(&dashboard);
    let form = format!(
        "renovate=on&renovate_author=renovate-bot&packaging=on&packaging_type=deb\
         &manifest_files=debian%2Fcontrol&upstream_repo=o%2Fflaky&upstream_forge=other{servers}"
    );
    let saved = dashboard
        .post("/settings/projects/forged/maintenance", &form)
        .await;
    assert!(
        saved.body.contains("The upstream was not checked"),
        "{}",
        saved.body
    );
    assert!(
        dashboard
            .store
            .project_maintenance("forged")
            .unwrap()
            .packaging
            .is_some()
    );
}

#[tokio::test]
async fn an_unknown_upstream_token_forge_is_a_400_before_any_forge_call() {
    let dashboard = forgejo_maintenance_dashboard().await;
    let page = dashboard.get("/settings/projects/forged").await;
    let csrf = scraper::Html::parse_document(&page.body)
        .select(
            &scraper::Selector::parse("form[action$=\"/maintenance\"] input[name=csrf]").unwrap(),
        )
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the maintenance form carries a csrf token")
        .to_owned();
    for action in ["", "/clear"] {
        let reply = dashboard
            .post(
                &format!("/settings/projects/forged/upstream-token{action}?forge=nope"),
                &format!("token_nope=x&csrf={csrf}"),
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{action}");
    }
}

#[tokio::test]
async fn a_github_project_can_name_a_forgejo_upstream() {
    let dashboard = forgejo_maintenance_dashboard().await;
    dashboard
        .store
        .add_collaborator("maestro", USER, "test")
        .unwrap();
    let servers = forgejo_server_fields(&dashboard);
    let reply = dashboard
        .post(
            "/settings/projects/maestro/maintenance/check",
            &format!(
                "renovate=on&packaging=on&packaging_type=deb&manifest_files=debian%2Fcontrol\
                 &upstream_repo=o%2Ftool&upstream_forge=other{servers}"
            ),
        )
        .await;
    assert!(
        reply
            .body
            .contains("Checked with no token: o/tool is readable on other."),
        "{}",
        reply.body
    );
}

const RENOVATE_ROUTE: &str = "/settings/projects/maestro/renovate-agent";

async fn renovate_csrf(dashboard: &Dashboard) -> String {
    let page = dashboard.get(PROJECT_PAGE).await;
    let doc = scraper::Html::parse_document(&page.body);
    doc.select(&scraper::Selector::parse("form.renovate-agent-form input[name=csrf]").unwrap())
        .next()
        .and_then(|input| input.value().attr("value"))
        .expect("the Renovate agent card carries a csrf token")
        .to_owned()
}

async fn renovate_post(dashboard: &Dashboard, fields: &str) -> Reply {
    let csrf = renovate_csrf(dashboard).await;
    dashboard
        .post(RENOVATE_ROUTE, &format!("csrf={csrf}&{fields}"))
        .await
}

fn renovate_card_checked(body: &str, selector: &str) -> Vec<String> {
    let doc = scraper::Html::parse_document(body);
    let sel = scraper::Selector::parse(&format!("form.renovate-agent-form {selector}")).unwrap();
    doc.select(&sel)
        .map(|input| input.value().attr("value").unwrap_or("").to_owned())
        .collect()
}

#[tokio::test]
async fn the_renovate_agent_card_is_off_and_shows_the_review_stage_by_default() {
    let dashboard = dashboard();
    dashboard
        .store
        .save_project_efforts(
            "maestro",
            &efforts("medium", "medium", "medium", "medium", "high"),
            &agents("claude", "claude", "claude", "claude", "claude"),
        )
        .unwrap();
    let body = dashboard.get(PROJECT_PAGE).await.body;
    assert_eq!(
        renovate_card_checked(&body, "input[name=renovate_override][checked]"),
        Vec::<String>::new()
    );
    assert_eq!(
        renovate_card_checked(&body, "input[name=renovate_agent][checked]"),
        ["claude"]
    );
    assert_eq!(
        renovate_card_checked(&body, "input[name=renovate_effort][checked]"),
        ["high"]
    );
}

#[tokio::test]
async fn saving_the_renovate_agent_round_trips_and_turning_it_off_deletes_it() {
    let dashboard = dashboard();
    let reply = renovate_post(
        &dashboard,
        "renovate_override=on&renovate_agent=codex&renovate=gpt-5&renovate_effort=low",
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.store.project_renovate_agent("maestro").unwrap(),
        Some(factory_server::RenovateAgent {
            agent: "codex".into(),
            model: Some("gpt-5".into()),
            effort: "low".into(),
        })
    );
    let body = dashboard.get(PROJECT_PAGE).await.body;
    assert_eq!(
        renovate_card_checked(&body, "input[name=renovate_override][checked]"),
        ["on"]
    );
    assert_eq!(
        renovate_card_checked(&body, "input[name=renovate_agent][checked]"),
        ["codex"]
    );
    assert_eq!(
        renovate_card_checked(&body, "input[name=renovate][value=gpt-5]"),
        ["gpt-5"]
    );
    assert_eq!(
        renovate_card_checked(&body, "input[name=renovate_effort][checked]"),
        ["low"]
    );

    let reply = renovate_post(&dashboard, "renovate_agent=codex&renovate_effort=low").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.store.project_renovate_agent("maestro").unwrap(),
        None
    );
}

#[tokio::test]
async fn a_bad_renovate_agent_is_rejected_by_name_and_stores_nothing() {
    let dashboard = dashboard();
    for fields in [
        "renovate_override=on&renovate_agent=gemini&renovate_effort=low",
        "renovate_override=on&renovate_agent=codex&renovate_effort=max",
        "renovate_override=on&renovate_agent=local&renovate=&renovate_effort=high",
    ] {
        let reply = renovate_post(&dashboard, fields).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{fields}");
        assert!(reply.body.contains("Renovate"), "{fields}");
        assert_eq!(
            dashboard.store.project_renovate_agent("maestro").unwrap(),
            None,
            "{fields}"
        );
    }
}

#[tokio::test]
async fn the_renovate_agent_route_needs_a_csrf_token() {
    let dashboard = dashboard();
    let reply = dashboard
        .post(
            RENOVATE_ROUTE,
            "renovate_override=on&renovate_agent=claude&renovate_effort=low",
        )
        .await;
    assert_ne!(reply.status, StatusCode::OK);
    assert_eq!(
        dashboard.store.project_renovate_agent("maestro").unwrap(),
        None
    );
}

#[tokio::test]
async fn a_non_collaborator_cannot_read_or_save_the_renovate_agent() {
    let dashboard = dashboard();
    let csrf = renovate_csrf(&dashboard).await;
    let page = dashboard.get_as(STRANGER, PROJECT_PAGE).await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    let reply = dashboard
        .post_as(
            STRANGER,
            RENOVATE_ROUTE,
            &format!("csrf={csrf}&renovate_override=on&renovate_agent=claude&renovate_effort=low"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        dashboard.store.project_renovate_agent("maestro").unwrap(),
        None
    );
}

#[tokio::test]
async fn the_renovate_agent_and_the_stage_panel_save_independently() {
    let dashboard = dashboard();
    let stages = (
        dashboard.store.project_agents("maestro").unwrap(),
        dashboard.store.project_efforts("maestro").unwrap(),
        dashboard
            .store
            .project_models("maestro", &StageModels::default())
            .unwrap(),
    );
    renovate_post(
        &dashboard,
        "renovate_override=on&renovate_agent=codex&renovate=gpt-5&renovate_effort=low",
    )
    .await;
    assert_eq!(
        stages,
        (
            dashboard.store.project_agents("maestro").unwrap(),
            dashboard.store.project_efforts("maestro").unwrap(),
            dashboard
                .store
                .project_models("maestro", &StageModels::default())
                .unwrap(),
        )
    );

    let reply = dashboard
        .post(
            "/settings/projects/maestro",
            "clarify=&spec=&plan=&implement=&review=&\
             clarify_effort=medium&spec_effort=medium&plan_effort=medium&\
             implement_effort=medium&review_effort=medium",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        dashboard
            .store
            .project_renovate_agent("maestro")
            .unwrap()
            .is_some()
    );
}
