use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::{
    AgentResult, Ask, Event, Gates, PlannedPr, PrDescription, Question, Stage, Task, Turn,
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            location,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
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
async fn the_index_lists_jobs_and_those_waiting_for_the_user() {
    let dashboard = dashboard();
    let page = dashboard.get("/").await;
    assert!(page.body.contains("<option selected>maestro</option>"));
    assert!(page.body.contains("name=\"project\" value=\"maestro\""));
    assert!(page.body.contains("No jobs yet."));

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
async fn both_job_lists_show_only_the_selected_project() {
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
    assert_eq!(page.body.matches("Add a reboot button</a>").count(), 2);
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
    assert!(body.contains("The publish gate has no effect yet."));
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
