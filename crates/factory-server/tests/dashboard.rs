use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::{
    AgentResult, Ask, BranchSummary, Event, Gates, PlannedPr, PrDescription, Question, Stage, Task,
    Turn,
};
use factory_server::{Projects, Store, StoreError, USER_HEADER, app};
use http_body_util::BodyExt;
use tokio::sync::Notify;
use tower::ServiceExt;

struct Dashboard {
    store: Arc<Store>,
    router: Router,
    data: tempfile::TempDir,
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
    let dir = tempfile::tempdir().unwrap();
    write_project(
        dir.path(),
        "maestro",
        &format!("repo = \"{repo}\"\nbase = \"main\"\n"),
    );
    let projects = Arc::new(Projects::load(dir.path()).unwrap());
    let store = Arc::new(Store::open_in_memory().unwrap());
    let data = tempfile::tempdir().unwrap();
    Dashboard {
        router: app(
            store.clone(),
            projects,
            Arc::new(Notify::new()),
            data.path().to_owned(),
        ),
        store,
        data,
    }
}

fn write_project(dir: &Path, name: &str, toml: &str) {
    fs::create_dir_all(dir.join(name)).unwrap();
    fs::write(dir.join(name).join("project.toml"), toml).unwrap();
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
    assert!(page.body.contains("Nothing needs you right now."));

    to_spec_approval(&dashboard, id);
    let page = dashboard.get("/").await;
    assert!(!page.body.contains("Nothing needs you right now."));
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
    assert!(page.body.contains(" UTC · $1.25</p>"));
}

#[tokio::test]
async fn a_job_without_cost_shows_a_dash() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains(" UTC · –</p>"));
}

#[tokio::test]
async fn a_cancelled_job_still_shows_its_cost() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.store.add_cost(id, 2.0).unwrap();
    dashboard.post(&format!("/jobs/{id}/cancel"), "").await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("cancelled"));
    assert!(page.body.contains(" UTC · $2.00</p>"));
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

#[test]
fn the_repository_project_files_load() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../projects");
    let projects = Projects::load(&dir).unwrap();
    assert_eq!(
        projects.get("maestro").unwrap().repo,
        "Badger-Embedded/maestro"
    );
}

#[test]
fn a_project_file_with_an_unknown_key_fails_to_load() {
    let dir = tempfile::tempdir().unwrap();
    write_project(
        dir.path(),
        "x",
        "repo = \"a/b\"\nbase = \"main\"\nbranch = \"dev\"\n",
    );
    let err = Projects::load(dir.path()).unwrap_err();
    assert!(err.to_string().contains("project.toml"));
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

#[tokio::test]
async fn the_job_page_shows_the_log_and_reloads_while_the_agent_works() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Start);
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

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("Bash: cargo test"));
    assert!(page.body.contains("http-equiv=\"refresh\""));

    to_spec_approval_from_clarifying(&dashboard, id);
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(!page.body.contains("http-equiv=\"refresh\""));
}

#[tokio::test]
async fn the_job_page_keeps_the_log_scrolled_only_where_the_log_is_shown() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;

    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(!page.body.contains("<script"));

    dashboard.apply(id, Event::Start);
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
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert_eq!(page.body.matches("<script>").count(), 1);
    assert!(page.body.contains("sessionStorage"));
    assert!(page.body.contains("http-equiv=\"refresh\" content=\"5\""));

    assert!(!dashboard.get("/").await.body.contains("sessionStorage"));
}

fn to_spec_approval_from_clarifying(dashboard: &Dashboard, id: i64) {
    dashboard.apply(id, Event::Agent(AgentResult::Clarified));
    dashboard.apply(id, Event::Agent(AgentResult::Spec("spec".into())));
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
            auto_merge: false,
            gates: Gates {
                spec: false,
                plan: false,
                publish: false
            },
        }
    );
    assert!(page.body.contains("name=\"auto_merge\""));

    dashboard
        .post(
            "/settings",
            "max_workers=2&poll_seconds=60&worker_image=i&worker_memory=3g&worker_cpus=4&auto_merge=on",
        )
        .await;
    assert!(dashboard.store.settings().unwrap().auto_merge);
    assert!(dashboard.get("/settings").await.body.contains("checked"));
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
    assert!(!index.body.contains("Nothing needs you right now."));

    dashboard
        .store
        .save_settings(&factory_server::Settings {
            auto_merge: true,
            ..factory_server::Settings::default()
        })
        .unwrap();
    let page = dashboard.get(&format!("/jobs/{id}")).await;
    assert!(page.body.contains("merges when green"));
    assert!(page.body.contains("Auto-merge is on"));
    let index = dashboard.get("/").await;
    assert!(index.body.contains("Nothing needs you right now."));
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
            .contains(r#"<a href="https://github.com/Badger-Embedded/maestro/pull/7">#7</a>"#)
    );
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
    );
    let page = Dashboard {
        store: dashboard.store.clone(),
        router,
        data: tempfile::tempdir().unwrap(),
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
    let dir = tempfile::tempdir().unwrap();
    for name in ["maestro", "tool"] {
        write_project(dir.path(), name, "repo = \"o/r\"\nbase = \"main\"\n");
    }
    let projects = Arc::new(Projects::load(dir.path()).unwrap());
    let store = Arc::new(Store::open_in_memory().unwrap());
    let data = tempfile::tempdir().unwrap();
    Dashboard {
        router: app(
            store.clone(),
            projects,
            Arc::new(Notify::new()),
            data.path().to_owned(),
        ),
        store,
        data,
    }
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
    assert!(page.body.contains("Nothing needs you right now."));
    assert!(page.body.contains("New request for tool"));
    assert!(page.body.contains("name=\"project\" value=\"tool\""));

    let page = dashboard.get("/?project=maestro").await;
    assert!(!page.body.contains("Tool change"));
    assert!(!page.body.contains("Nothing needs you right now."));
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
    assert!(index.body.contains("Nothing needs you right now."));

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
    assert!(page.body.contains(&format!("/jobs/{id}/approve")));
    assert!(page.body.contains(&format!("/jobs/{id}/changes")));
    let index = dashboard.get("/").await;
    assert!(!index.body.contains("Nothing needs you right now."));
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

/// The text of the lane `class` on the index page, up to the next lane.
fn lane_html<'a>(body: &'a str, class: &str) -> &'a str {
    let marker = format!("class=\"lane {class}\"");
    let start = body.find(&marker).unwrap_or_else(|| panic!("no {class}"));
    let rest = &body[start + marker.len()..];
    &rest[..rest.find("class=\"lane ").unwrap_or(rest.len())]
}

/// The lanes that list job `id`.
fn lanes_of(body: &str, id: i64) -> Vec<&'static str> {
    let link = format!("href=\"/jobs/{id}\"");
    ["lane-you", "lane-agent", "lane-factory", "lane-done"]
        .into_iter()
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
    assert_lane(&dashboard, id, "lane-factory").await;

    let steps = [
        (Event::Start, "lane-agent"),
        (
            Event::Agent(AgentResult::Questions(vec![Question {
                text: "Which?".into(),
            }])),
            "lane-you",
        ),
        (Event::Answer(vec!["This".into()]), "lane-agent"),
        (Event::Agent(AgentResult::Clarified), "lane-agent"),
        (Event::Agent(AgentResult::Spec("Goal.".into())), "lane-you"),
        (Event::Approve, "lane-agent"),
        (Event::Agent(AgentResult::Plan(plan_of_one())), "lane-you"),
        (Event::Approve, "lane-agent"),
        (
            Event::Agent(AgentResult::CommitsReady(ready())),
            "lane-factory",
        ),
        (Event::ChecksPassed, "lane-factory"),
        (Event::Published { number: 7 }, "lane-you"),
        (
            Event::ReviewComments {
                comments: "Rename it".into(),
            },
            "lane-agent",
        ),
        (
            Event::Agent(AgentResult::CommitsReady(ready())),
            "lane-factory",
        ),
        (Event::ChecksPassed, "lane-factory"),
        (Event::Published { number: 7 }, "lane-you"),
        (Event::Merged, "lane-done"),
    ];
    for (event, lane) in steps {
        dashboard.apply(id, event.clone());
        let stage = dashboard.store.get(id).unwrap().job.stage().clone();
        let body = dashboard.get("/").await.body;
        assert_eq!(lanes_of(&body, id), [lane], "after {event:?} in {stage:?}");
    }
}

#[tokio::test]
async fn failed_and_cancelled_jobs_land_in_done() {
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
    assert_lane(&dashboard, failed, "lane-done").await;
    assert_lane(&dashboard, cancelled, "lane-done").await;
}

#[tokio::test]
async fn the_publish_gate_waits_in_factory_then_you() {
    let dashboard = dashboard();
    let id = at_publish_gate(&dashboard).await;
    assert_lane(&dashboard, id, "lane-factory").await;
    dashboard.apply(id, Event::Summarized(branch_summary()));
    assert_lane(&dashboard, id, "lane-you").await;
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
    assert_lane(&dashboard, id, "lane-you").await;
    dashboard
        .store
        .save_settings(&factory_server::Settings {
            auto_merge: true,
            ..factory_server::Settings::default()
        })
        .unwrap();
    assert_lane(&dashboard, id, "lane-factory").await;
}

#[tokio::test]
async fn the_board_replaces_the_two_lists() {
    let body = dashboard().get("/").await.body;
    assert!(!body.contains("Waiting for you"));
    assert!(!body.contains("All jobs"));
    assert!(!body.contains("No jobs yet."));
    for title in ["You", "Agent", "Factory", "Done"] {
        assert!(
            body.contains(&format!("{title} <span class=\"count\">0</span>")),
            "{title}"
        );
    }
}

#[tokio::test]
async fn empty_lanes_say_so() {
    let body = dashboard().get("/").await.body;
    for (class, message) in [
        ("lane-you", "Nothing needs you right now."),
        ("lane-agent", "No agent is working."),
        ("lane-factory", "The factory has nothing to do."),
        ("lane-done", "No finished jobs."),
    ] {
        assert!(lane_html(&body, class).contains(message), "{class}");
    }

    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Cancel);
    let body = dashboard.get("/").await.body;
    assert!(!lane_html(&body, "lane-done").contains("No finished jobs."));
    assert!(lane_html(&body, "lane-factory").contains("The factory has nothing to do."));
}

#[tokio::test]
async fn lanes_show_their_job_counts() {
    let dashboard = dashboard();
    let a = dashboard.new_job().await;
    let b = dashboard.new_job().await;
    let c = dashboard.new_job().await;
    to_spec_approval(&dashboard, a);
    to_spec_approval(&dashboard, b);
    dashboard.apply(c, Event::Cancel);
    let body = dashboard.get("/").await.body;
    assert!(body.contains("You <span class=\"count\">2</span>"));
    assert!(body.contains("Agent <span class=\"count\">0</span>"));
    assert!(body.contains("Factory <span class=\"count\">0</span>"));
    assert!(body.contains("Done <span class=\"count\">1</span>"));
}

#[tokio::test]
async fn done_is_a_collapsed_details_element_that_shows_its_count() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    dashboard.apply(id, Event::Cancel);
    let body = dashboard.get("/").await.body;
    assert!(body.contains(
        "<details class=\"lane lane-done\"><summary>Done <span class=\"count\">1</span></summary>"
    ));
    assert!(!body.contains("<details open"));
    assert!(!body.contains("details class=\"lane lane-done\" open"));
    for other in ["lane-you", "lane-agent", "lane-factory"] {
        assert!(body.contains(&format!("<section class=\"lane {other}\">")));
    }
}

#[tokio::test]
async fn each_lane_lists_the_newest_job_first() {
    let dashboard = dashboard();
    let first = dashboard.new_job().await;
    let second = dashboard.new_job().await;
    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "lane-factory");
    let older = lane.find(&format!("href=\"/jobs/{first}\"")).unwrap();
    let newer = lane.find(&format!("href=\"/jobs/{second}\"")).unwrap();
    assert!(newer < older);
}

#[tokio::test]
async fn cards_keep_the_content_of_the_old_rows() {
    let dashboard = dashboard();
    let id = dashboard.new_job().await;
    let body = dashboard.get("/").await.body;
    let lane = lane_html(&body, "lane-factory");
    assert!(lane.contains(&format!("#{id}")));
    assert!(lane.contains("Add a reboot button</a>"));
    assert!(lane.contains("class=\"badge factory\">queued"));
    assert!(lane.contains("maestro · "));
    assert!(lane.contains("alice@example.com"));
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
    assert!(lane_html(&body, "lane-you").contains("<form"));
}

#[tokio::test]
async fn the_board_counts_only_the_selected_projects_jobs() {
    let dashboard = two_projects();
    dashboard.new_job().await;
    dashboard.post_job("tool", "Tool change", &[]).await;
    dashboard.post_job("tool", "Another tool change", &[]).await;
    let body = dashboard.get("/?project=tool").await.body;
    assert!(body.contains("Factory <span class=\"count\">2</span>"));
    let body = dashboard.get("/?project=maestro").await.body;
    assert!(body.contains("Factory <span class=\"count\">1</span>"));
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
