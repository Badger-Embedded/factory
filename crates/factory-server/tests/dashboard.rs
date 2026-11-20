use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::{
    AgentResult, Ask, BranchSummary, DEFAULT_EFFORT, Event, Gates, MAX_FAILED_ROUNDS,
    MAX_REVIEW_ROUNDS, PlanUsage, PlannedPr, PrDescription, Question, Stage, Task, Turn,
};
use factory_server::{
    BranchInfo, GitHubRepos, Permission, PlanUsageCache, Projects, Protection, RepoChecker,
    RepoInfo, StageEfforts, StageModels, Store, StoreError, USER_HEADER, app,
};
use http_body_util::BodyExt;
use tokio::sync::Notify;
use tower::ServiceExt;

/// A fake GitHub the onboarding wizard checks against, so tests need no network.
/// The default passes every check; a test mutates one field to fail one.
#[derive(Clone)]
struct FakeRepos {
    repo: Option<RepoInfo>,
    branch: Option<BranchInfo>,
    ci: bool,
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
        }
    }
}

impl GitHubRepos for FakeRepos {
    fn repo(&self, _repo: &str) -> impl Future<Output = Result<Option<RepoInfo>, String>> + Send {
        std::future::ready(Ok(self.repo.clone()))
    }

    fn branch(
        &self,
        _repo: &str,
        _branch: &str,
    ) -> impl Future<Output = Result<Option<BranchInfo>, String>> + Send {
        std::future::ready(Ok(self.branch.clone()))
    }

    fn has_ci(&self, _repo: &str) -> impl Future<Output = Result<bool, String>> + Send {
        std::future::ready(Ok(self.ci))
    }
}

struct Dashboard {
    store: Arc<Store>,
    router: Router,
    data: tempfile::TempDir,
    usage: Arc<PlanUsageCache>,
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
    body: String,
}

const BOUNDARY: &str = "factory-test-boundary";

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
const GIF: &[u8] = b"GIF89a0000";
const WEBP: &[u8] = b"RIFF\x04\0\0\0WEBPVP8 ";

fn dashboard() -> Dashboard {
    dashboard_for("Badger-Embedded/maestro")
}

fn dashboard_for(repo: &str) -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.create_project("maestro", repo, "main", None).unwrap();
    dashboard_with(store)
}

/// A dashboard reading projects from `store`, so a project onboarded in the
/// store is visible on the next request. Its onboarding checks see a fit
/// repository.
fn dashboard_with(store: Arc<Store>) -> Dashboard {
    dashboard_with_github(store, Arc::new(FakeRepos::fit()))
}

/// As [`dashboard_with`], with the onboarding checks answered by `github`, so a
/// test can fail one check.
fn dashboard_with_github(store: Arc<Store>, github: Arc<dyn RepoChecker>) -> Dashboard {
    let projects = Arc::new(Projects::from_store(Arc::clone(&store)));
    let data = tempfile::tempdir().unwrap();
    let usage = Arc::new(PlanUsageCache::new());
    Dashboard {
        router: app(
            store.clone(),
            projects,
            Arc::new(Notify::new()),
            data.path().to_owned(),
            Arc::clone(&usage),
            github,
        ),
        store,
        data,
        usage,
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            location,
            content_type,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.send(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    async fn post(&self, uri: &str, form: &str) -> Reply {
        self.send(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(USER_HEADER, "alice@example.com")
                .body(Body::from(form.to_owned()))
                .unwrap(),
        )
        .await
    }

    async fn post_job(&self, project: &str, request: &str, images: &[(&str, &[u8])]) -> Reply {
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
                .header(USER_HEADER, "alice@example.com")
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
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    let empty = dashboard.post_job("maestro", "  ", &[]).await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
}

#[tokio::test]
async fn the_index_lists_jobs_and_flags_those_waiting_for_the_user() {
    let dashboard = dashboard();
    let page = dashboard.get("/").await;
    assert!(page.body.contains("<option selected>maestro</option>"));
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
    for uri in ["/", "/settings", &format!("/jobs/{id}")] {
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
    dashboard.store.add_cost(id, 0.5).unwrap();
    dashboard.store.add_cost(id, 0.75).unwrap();

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(
        page.body
            .contains("<span class=\"badge cost\" id=\"job-cost\">$1.25</span>")
    );
    assert!(page.body.contains("requested by alice@example.com"));
    assert!(page.body.contains(" UTC</p>"));
    assert!(!page.body.contains(" UTC · "));
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
async fn a_cancelled_job_still_shows_its_cost() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 2.0).unwrap();
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
        .create_project("maestro", "o/one", "main", None)
        .unwrap();
    assert!(matches!(
        store.create_project("maestro", "o/two", "main", None),
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
        .create_project("maestro", "o/repo", "main", None)
        .unwrap();
    assert!(matches!(
        store.create_project("tool", "o/repo", "main", None),
        Err(StoreError::DuplicateRepo(repo)) if repo == "o/repo"
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
        .create_project("widgets", "acme/widgets", "main", None)
        .unwrap();

    // It shows on the board selector, the settings page and the new-job form
    // at once, and a job can be created against it.
    let index = dashboard.get("/?project=widgets").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("widgets"));
    assert!(dashboard.get("/settings").await.body.contains("widgets"));
    let created = dashboard.post_job("widgets", "A widget change", &[]).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
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
    let page = dashboard.get("/settings").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("name=\"max_workers\""));
    assert!(page.body.contains("value=\"2\""));
    assert!(dashboard.get("/").await.body.contains("href=\"/settings\""));

    let reply = dashboard
        .post(
            "/settings",
            "max_workers=4&poll_seconds=30&worker_image=localhost%2Fw%3A2&worker_memory=6g&worker_cpus=6",
        )
        .await;
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
            gates: Gates {
                spec: false,
                plan: false,
                publish: false
            },
        }
    );
}

#[tokio::test]
async fn auto_merge_is_a_per_project_setting() {
    let dashboard = dashboard();
    // The per-project form carries the auto-merge toggle, the global form
    // does not.
    let page = dashboard.get("/settings").await;
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
    assert!(dashboard.get("/settings").await.body.contains("checked"));

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
    let page = dashboard.get("/settings").await;
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
    for form in [
        "max_workers=2&poll_seconds=5&worker_image=i&worker_memory=3g&worker_cpus=4",
        "max_workers=2&poll_seconds=60&worker_image=+&worker_memory=3g&worker_cpus=4",
        "max_workers=-1&poll_seconds=60&worker_image=i&worker_memory=3g&worker_cpus=4",
    ] {
        let reply = dashboard.post("/settings", form).await;
        assert!(reply.status.is_client_error(), "{form}: {}", reply.status);
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
    dashboard.apply(id, Event::StartReview);
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
            dashboard.apply(id, Event::StartReview);
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
            .contains(r#"<a href="https://github.com/Badger-Embedded/maestro">maestro</a>"#)
    );
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
    assert_eq!(page.body.matches("target=").count(), 1);
    assert_eq!(page.body.matches("noopener").count(), 1);
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

#[tokio::test]
async fn a_job_whose_project_is_gone_renders_without_links() {
    let dashboard = dashboard();
    let id = with_plan(&dashboard, vec![planned("first", Some(7))]).await;
    let router = app(
        dashboard.store.clone(),
        Arc::new(Projects::default()),
        Arc::new(Notify::new()),
        dashboard.data.path().to_owned(),
        Arc::new(PlanUsageCache::new()),
        Arc::new(FakeRepos::fit()),
    );
    let page = Dashboard {
        store: dashboard.store.clone(),
        router,
        data: tempfile::tempdir().unwrap(),
        usage: Arc::new(PlanUsageCache::new()),
    }
    .get(&format!("/jobs/{id}"))
    .await;

    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("maestro"));
    assert!(page.body.contains("#7"));
    assert!(!page.body.contains("github.com"));
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

fn two_projects() -> Dashboard {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .create_project("maestro", "o/maestro", "main", None)
        .unwrap();
    store
        .create_project("tool", "o/tool", "main", None)
        .unwrap();
    dashboard_with(store)
}

#[tokio::test]
async fn the_index_defaults_to_the_first_project() {
    let dashboard = two_projects();
    dashboard.new_job().await;
    dashboard.post_job("tool", "Tool change", &[]).await;
    let page = dashboard.get("/").await;
    assert!(page.body.contains("<option selected>maestro</option>"));
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
    assert!(page.body.contains("<option selected>tool</option>"));
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
async fn an_unknown_project_in_the_index_is_a_bad_request() {
    let page = dashboard().get("/?project=nope").await;
    assert_eq!(page.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_selector_is_in_the_topbar_and_the_form_has_no_project_select() {
    let page = two_projects().get("/?project=tool").await;
    let main = page.body.find("<main>").unwrap();
    let (topbar, rest) = page.body.split_at(main);
    assert!(topbar.contains("<select id=\"project\" name=\"project\">"));
    assert!(topbar.contains("<option>maestro</option>"));
    assert!(topbar.contains("<option selected>tool</option>"));
    assert!(topbar.contains("<label class=\"visually-hidden\" for=\"project\">Project</label>"));
    assert!(topbar.contains("<noscript><button>Go</button></noscript>"));
    assert!(!rest.contains("<select"));
    assert!(rest.contains("<input type=\"hidden\" name=\"project\" value=\"tool\">"));
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
async fn an_image_is_checked_by_content_not_name_or_type() {
    let dashboard = dashboard();
    let reply = dashboard
        .post_job("maestro", "x", &[("photo.png", b"<svg></svg>")])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("PNG, JPEG, GIF or WebP"));
    assert_eq!(dashboard.store.list().unwrap().len(), 0);

    let disguised = dashboard
        .post_job("maestro", "x", &[("photo.txt", PNG)])
        .await;
    assert_eq!(disguised.status, StatusCode::SEE_OTHER);
    assert_eq!(
        dashboard.store.images(job_id(&disguised)).unwrap(),
        ["image-1.png"]
    );
}

#[tokio::test]
async fn an_oversized_image_is_rejected_with_the_limit() {
    let dashboard = dashboard();
    let mut big = PNG.to_vec();
    big.resize(factory_server::MAX_IMAGE_BYTES + 1, 0);
    let reply = dashboard
        .post_job("maestro", "x", &[("big.png", &big)])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply
            .body
            .contains(&factory_server::MAX_IMAGE_BYTES.to_string())
    );
    assert_eq!(dashboard.store.list().unwrap().len(), 0);

    let mut exact = PNG.to_vec();
    exact.resize(factory_server::MAX_IMAGE_BYTES, 0);
    let reply = dashboard
        .post_job("maestro", "x", &[("ok.png", &exact)])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn too_many_images_are_rejected_with_the_limit() {
    let dashboard = dashboard();
    let images: Vec<(&str, &[u8])> = vec![("a.png", PNG); factory_server::MAX_IMAGES + 1];
    let reply = dashboard.post_job("maestro", "x", &images).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply
            .body
            .contains(&format!("at most {} images", factory_server::MAX_IMAGES))
    );
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
    assert!(!dashboard.data.path().join("jobs").exists());

    let images = &images[..factory_server::MAX_IMAGES];
    let reply = dashboard.post_job("maestro", "x", images).await;
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

fn gate_form(gates: &str) -> String {
    format!("max_workers=2&poll_seconds=60&worker_image=i&worker_memory=3g&worker_cpus=4{gates}")
}

#[tokio::test]
async fn the_settings_page_has_a_checkbox_per_gate_with_spec_and_plan_on() {
    let dashboard = dashboard();
    let body = dashboard.get("/settings").await.body;
    assert!(body.contains("Approval gates"));
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
        dashboard.post("/settings", &all).await.status,
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
        .post("/settings", &gate_form("&gate_plan=on"))
        .await;
    assert_eq!(
        dashboard.store.settings().unwrap().gates,
        Gates {
            spec: false,
            plan: true,
            publish: false
        }
    );

    dashboard.post("/settings", &gate_form("")).await;
    let gates = dashboard.store.settings().unwrap().gates;
    assert_eq!(
        gates,
        Gates {
            spec: false,
            plan: false,
            publish: false
        }
    );
    let body = dashboard.get("/settings").await.body;
    assert!(!body.contains(r#"name="gate_spec" value="on" checked"#));
    assert!(!body.contains(r#"name="gate_plan" value="on" checked"#));
}

#[tokio::test]
async fn the_job_page_shows_the_gates_the_job_started_with() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let page = dashboard.get(&format!("/jobs/{first}")).await;
    assert!(page.body.contains("Approval gates: spec, plan"));

    dashboard
        .post("/settings", &gate_form("&gate_publish=on"))
        .await;
    let second = dashboard.new_job().await;
    let page = dashboard.get(&format!("/jobs/{second}")).await;
    assert!(page.body.contains("Approval gates: publish"));
    let page = dashboard.get(&format!("/jobs/{first}")).await;
    assert!(page.body.contains("Approval gates: spec, plan"));

    dashboard.post("/settings", &gate_form("")).await;
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
            .contains(r#"<label class="file-button" for="images">Attach images</label>"#)
    );
    assert!(page.body.contains(r#"class="visually-hidden">"#));
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
        .save_settings(&factory_server::Settings {
            gates: Gates {
                spec: false,
                plan: false,
                publish: true,
            },
            ..factory_server::Settings::default()
        })
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
    let older = &done[done.find("<details class=\"older\">").unwrap()..];
    assert!(older.contains("<summary>Show 2 older</summary>"));
    // Newest first, so the two oldest jobs are the collapsed ones.
    for id in &ids[..2] {
        assert!(older.contains(&format!("href=\"/jobs/{id}\"")), "{id}");
    }
    for id in &ids[2..] {
        assert!(!older.contains(&format!("href=\"/jobs/{id}\"")), "{id}");
    }
    assert!(!done.contains("<details class=\"older\" open"));
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
    dashboard.store.add_cost(first, 1.0).unwrap();
    dashboard.store.add_cost(second, 2.5).unwrap();

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
    dashboard.store.add_cost(id, 2.0).unwrap();

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
    dashboard.store.add_cost(id, 0.5).unwrap();
    dashboard.store.add_cost(id, 0.75).unwrap();

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
    dashboard.store.add_cost(id, 1.0).unwrap();

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
    assert_eq!(medium.clarify, DEFAULT_EFFORT);
    assert_eq!(medium.review, DEFAULT_EFFORT);
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
        store.save_project_efforts("maestro", &saved).unwrap();
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
        .save_project_efforts("maestro", &efforts("high", "high", "high", "high", "high"))
        .unwrap();
    store
        .save_project_efforts("factory", &efforts("low", "low", "low", "low", "low"))
        .unwrap();
    store
        .save_project_efforts("maestro", &efforts("max", "max", "max", "max", "max"))
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
    let err = store.save_project_efforts("maestro", &bad).unwrap_err();
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
    store.save_project_efforts("maestro", &saved).unwrap();
    assert_eq!(store.project_efforts("maestro").unwrap(), saved);
}

#[tokio::test]
async fn the_settings_form_has_an_effort_selector_per_stage_preselected_to_the_effective_value() {
    let dashboard = dashboard();
    let page = dashboard.get("/settings").await;
    // A select named "<stage>_effort" sits beside each stage's model field.
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
    assert_eq!(page.body.matches("value=\"medium\" selected").count(), 5);

    // A saved level preselects in place of the default.
    dashboard
        .store
        .save_project_efforts("maestro", &efforts("low", "medium", "high", "xhigh", "max"))
        .unwrap();
    let page = dashboard.get("/settings").await;
    assert!(page.body.contains("value=\"low\" selected"));
    assert!(page.body.contains("value=\"high\" selected"));
    assert!(page.body.contains("value=\"xhigh\" selected"));
    assert!(page.body.contains("value=\"max\" selected"));
}

#[tokio::test]
async fn saving_the_form_persists_model_and_effort_and_leaves_other_projects_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    store
        .create_project("maestro", "Badger-Embedded/maestro", "main", None)
        .unwrap();
    store
        .create_project("tool", "Badger-Embedded/tool", "main", None)
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
    let page = dashboard.get("/settings").await.body;
    assert!(page.contains("action=\"/settings/projects/maestro\""));
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
        .post(
            "/settings",
            "max_workers=2&poll_seconds=60&worker_image=i&worker_memory=3g&worker_cpus=4",
        )
        .await;
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
    dashboard.store.add_cost(id, 1.25).unwrap();
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
        .create_project("maestro", "Badger-Embedded/maestro", "main", None)
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
async fn a_failed_job_still_counts_as_finished_for_the_30_day_expiry() {
    let dashboard = dashboard();
    let id = failed_out_of_rounds(&dashboard).await;
    // The Failed lane only changes where the board shows the job. The expiry
    // loop deletes it once it is 30 days old, because it is still finished.
    assert!(dashboard.store.get(id).unwrap().job.is_finished());
    assert_lane(&dashboard, id, "inbox-failed").await;
}

/// A `?skip_spend=1` body with Fable in `model_scoped`, using Unix timestamps
/// whose UTC dates the tile tests assert.
const USAGE_WITH_FABLE: &str = r#"{
    "five_hour": { "utilization": 0.42, "resets_at": 1700000000 },
    "seven_day": { "utilization": 0.73, "resets_at": 1701000000 },
    "model_scoped": [
        { "display_name": "Fable", "utilization": 0.1, "resets_at": 1701000000 }
    ]
}"#;

/// The same body with no `model_scoped`, so the plan reports no Fable window.
const USAGE_NO_FABLE: &str = r#"{
    "five_hour": { "utilization": 0.42, "resets_at": 1700000000 },
    "seven_day": { "utilization": 0.73, "resets_at": 1701000000 }
}"#;

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
    dashboard
        .usage
        .record_success(usage(USAGE_WITH_FABLE), now());

    let body = dashboard.get("/").await.body;
    let tile = usage_tile(&body).expect("the tile shows after a fresh success");
    assert!(tile.contains("Claude plan usage"));
    assert!(tile.contains("5-hour"));
    assert!(tile.contains("Weekly"));
    assert!(tile.contains("Fable"));
    assert!(tile.contains("42% used"));
    assert!(tile.contains("73% used"));
    assert!(tile.contains("resets 2023-11-14 22:13 UTC"));
    assert!(tile.contains("resets 2023-11-26 12:00 UTC"));
}

#[tokio::test]
async fn the_tile_shows_the_two_windows_when_the_plan_reports_no_fable() {
    let dashboard = dashboard();
    dashboard.usage.record_success(usage(USAGE_NO_FABLE), now());

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
async fn a_stale_success_hides_the_tile() {
    let dashboard = dashboard();
    let stale = now() - factory_core::PLAN_USAGE_STALE_AFTER_SECS - 1;
    dashboard
        .usage
        .record_success(usage(USAGE_WITH_FABLE), stale);

    let body = dashboard.get("/").await.body;
    assert!(usage_tile(&body).is_none());
    assert!(body.contains("id=\"board\""));
}

#[tokio::test]
async fn an_errored_fetch_hides_the_tile() {
    let dashboard = dashboard();
    dashboard
        .usage
        .record_success(usage(USAGE_WITH_FABLE), now());
    dashboard.usage.record_error();

    let body = dashboard.get("/").await.body;
    assert!(usage_tile(&body).is_none());
    assert!(body.contains("id=\"board\""));
}

#[tokio::test]
async fn the_tile_is_the_same_whatever_project_is_selected() {
    let dashboard = two_projects();
    dashboard
        .usage
        .record_success(usage(USAGE_WITH_FABLE), now());

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
    for url in [
        "/fragments/plan-usage",
        "/fragments/stats?project=maestro",
        "/fragments/board?project=maestro",
    ] {
        assert!(body.contains(&format!("hx-get=\"{url}\"")), "{url}");
    }
    // One trigger per region: the usage meters, the totals line and the job
    // rows.
    assert_eq!(body.matches("hx-trigger=\"every 5s\"").count(), 3);
    assert!(!body.contains("/fragments/lanes"));
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
            .contains("hx-get=\"/fragments/board?project=maestro\"")
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
async fn the_plan_usage_fragment_hides_and_reappears_with_the_cache() {
    let dashboard = dashboard();
    // An empty cache: the fragment is empty, so a swap removes the tile.
    let empty = dashboard.get("/fragments/plan-usage").await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(!empty.body.contains("Claude plan usage"));

    dashboard
        .usage
        .record_success(usage(USAGE_WITH_FABLE), now());
    let fresh = dashboard.get("/fragments/plan-usage").await.body;
    assert!(fresh.contains("<section class=\"plan-usage\">"));
    assert!(fresh.contains("Claude plan usage"));
    assert!(fresh.contains("Fable"));

    dashboard.usage.record_error();
    let errored = dashboard.get("/fragments/plan-usage").await.body;
    assert!(!errored.contains("Claude plan usage"));
}

// --- Onboarding wizard ---

/// A dashboard with no projects yet, its onboarding checks answered by `github`.
fn empty_dashboard(github: Arc<dyn RepoChecker>) -> (Arc<Store>, Dashboard) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let dashboard = dashboard_with_github(Arc::clone(&store), github);
    (store, dashboard)
}

#[tokio::test]
async fn the_onboard_page_is_linked_and_shows_step_one() {
    let dashboard = dashboard();
    assert!(dashboard.get("/").await.body.contains("href=\"/onboard\""));
    assert!(
        dashboard
            .get("/settings")
            .await
            .body
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
    assert!(dashboard.get("/settings").await.body.contains("widgets"));
    let created = dashboard.post_job("widgets", "A widget change", &[]).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
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

    let settings = dashboard.get("/settings").await.body;
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
        .create_project("widgets", "acme/old", "main", None)
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
        .create_project("other", "acme/widgets", "main", None)
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
            _repo: &str,
        ) -> impl Future<Output = Result<Option<RepoInfo>, String>> + Send {
            std::future::ready(Err("gh api repos/acme/widgets failed: HTTP 500".into()))
        }
        fn branch(
            &self,
            _repo: &str,
            _branch: &str,
        ) -> impl Future<Output = Result<Option<BranchInfo>, String>> + Send {
            std::future::ready(Err("unreachable".into()))
        }
        fn has_ci(&self, _repo: &str) -> impl Future<Output = Result<bool, String>> + Send {
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
    let page = dashboard.get("/settings").await;
    assert!(page.body.contains("MCP servers"));
    assert!(page.body.contains("No servers yet."));

    // Add one.
    let added = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fmcp.context7.com%2Fmcp&headers=Authorization",
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
            .headers,
        vec!["Authorization".to_owned()],
    );

    // Edit its URL path and headers. The name stays, so the route keys on it.
    let edited = dashboard
        .post(
            "/settings/mcp/context7",
            "url=https%3A%2F%2Fmcp.context7.com%2Fv2%2Fmcp&headers=",
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK);
    let server = dashboard.store.mcp_server("context7").unwrap().unwrap();
    assert_eq!(server.url, "https://mcp.context7.com/v2/mcp");
    assert_eq!(server.headers, Vec::<String>::new());

    // Delete it.
    let deleted = dashboard.post("/settings/mcp/context7/delete", "").await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert!(dashboard.store.mcp_server("context7").unwrap().is_none());
}

#[tokio::test]
async fn an_invalid_mcp_server_is_a_bad_request() {
    let dashboard = dashboard();
    // A non-https URL.
    let bad_url = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=http%3A%2F%2Fx.test%2Fmcp&headers=",
        )
        .await;
    assert_eq!(bad_url.status, StatusCode::BAD_REQUEST);
    // A reserved header name.
    let reserved = dashboard
        .post(
            "/settings/mcp",
            "name=context7&url=https%3A%2F%2Fx.test%2Fmcp&headers=Host",
        )
        .await;
    assert_eq!(reserved.status, StatusCode::BAD_REQUEST);
    // Nothing was stored.
    assert_eq!(dashboard.store.mcp_servers().unwrap(), Vec::new());
}
