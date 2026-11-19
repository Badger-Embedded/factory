use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_core::{AgentResult, Ask, Event, PlannedPr, Question, Stage, Task, Turn};
use factory_server::{Projects, Store, StoreError, USER_HEADER, app};
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Dashboard {
    store: Arc<Store>,
    router: Router,
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    body: String,
}

fn dashboard() -> Dashboard {
    let dir = tempfile::tempdir().unwrap();
    write_project(
        dir.path(),
        "maestro",
        "repo = \"Badger-Embedded/maestro\"\nbase = \"main\"\n",
    );
    let projects = Arc::new(Projects::load(dir.path()).unwrap());
    let store = Arc::new(Store::open_in_memory().unwrap());
    Dashboard {
        router: app(store.clone(), projects),
        store,
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

    async fn new_job(&self) -> i64 {
        let reply = self
            .post("/jobs", "project=maestro&request=Add+a+reboot+button")
            .await;
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
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("project=maestro&request=x"))
                .unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(dashboard.store.list().unwrap()[0].requested_by, "local");
}

#[tokio::test]
async fn bad_requests_create_no_job() {
    let dashboard = dashboard();
    let unknown = dashboard.post("/jobs", "project=nope&request=x").await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    let empty = dashboard.post("/jobs", "project=maestro&request=++").await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(dashboard.store.list().unwrap().len(), 0);
}

#[tokio::test]
async fn the_index_lists_jobs_and_those_waiting_for_the_user() {
    let dashboard = dashboard();
    let page = dashboard.get("/").await;
    assert!(page.body.contains("<option>maestro</option>"));
    assert!(page.body.contains("No jobs yet."));

    let id = dashboard.new_job().await;
    let page = dashboard.get("/").await;
    assert!(page.body.contains(&format!("href=\"/jobs/{id}\"")));
    assert!(page.body.contains("Nothing."));

    to_spec_approval(&dashboard, id);
    let page = dashboard.get("/").await;
    assert!(!page.body.contains("Nothing."));
    assert!(page.body.contains("you, spec approval"));
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
