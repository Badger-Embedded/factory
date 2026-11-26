//! Tests for the wait before a Local worker starts: the factory sends one
//! `max_tokens: 1` request to the server's chat endpoint and the worker starts
//! only once it answers. A fake `git` and `podman` record their calls, so a
//! test sees whether a checkout or a container started.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use factory_core::{Agent, Cli, Event};
use factory_runner::{
    Account, Expect, ForgeKind, Git, GitHub, LocalTarget, LogLine, RunReport, WorkerConfig,
};
use factory_server::{
    AccountSource, AgentRegistry, AgentRequest, ClaudeToken, CodexLogin, Executor, Forges, GhRepos,
    HttpLocalServerProbe, LocalKeyOwner, LocalRequest, LocalSecrets, MASTER_KEY_LEN,
    PlanUsageCache, PodmanExecutor, SecretStore, Settings, Store,
};
use tokio::sync::Notify;

mod support;

const KEY: &str = "sk-local-load-key-456";

type Seen = Arc<Mutex<Vec<(Option<String>, serde_json::Value)>>>;

#[derive(Clone)]
enum Answer {
    Ok,
    Never,
    Gated(Arc<Notify>),
    StalledError(StatusCode),
    StalledBody(StatusCode),
    Status(StatusCode, String),
}

#[derive(Clone)]
struct Shared {
    answer: Answer,
    delay: Duration,
    seen: Seen,
    arrived: Arc<Notify>,
}

struct Stub {
    arrived: Arc<Notify>,
    url: String,
    seen: Seen,
}

async fn stub(answer: Answer, delay: Duration) -> Stub {
    async fn chat(State(shared): State<Shared>, headers: HeaderMap, body: String) -> Response {
        let auth = headers
            .get(header::AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned());
        shared
            .seen
            .lock()
            .unwrap()
            .push((auth, serde_json::from_str(&body).unwrap()));
        shared.arrived.notify_one();
        tokio::time::sleep(shared.delay).await;
        match shared.answer {
            Answer::Ok => (StatusCode::OK, r#"{"choices":[]}"#).into_response(),
            Answer::Never => std::future::pending().await,
            Answer::Gated(gate) => {
                gate.notified().await;
                (StatusCode::OK, r#"{"choices":[]}"#).into_response()
            }
            Answer::StalledBody(status) | Answer::StalledError(status) => (
                status,
                Body::from_stream(tokio_stream::pending::<
                    Result<axum::body::Bytes, std::io::Error>,
                >()),
            )
                .into_response(),
            Answer::Status(status, body) => (status, body).into_response(),
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let arrived = Arc::new(Notify::new());
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(Shared {
            answer,
            delay,
            seen: Arc::clone(&seen),
            arrived: Arc::clone(&arrived),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Stub { arrived, url, seen }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<Store>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        support::executable(
            dir.path(),
            "git",
            &format!(
                "#!/bin/bash\necho git >> '{calls}'\n\
                 while [ \"$1\" = -c ]; do shift 2; done\n\
                 case \"$1\" in --git-dir=*) shift ;; esac\n\
                 case \"$1\" in\n\
                 clone) mkdir -p \"${{@: -1}}/.git\" ;;\n\
                 config) case \"$*\" in *--get-regexp*) exit 1 ;; esac ;;\n\
                 esac\n",
                calls = dir.path().join("calls").display()
            ),
        );
        support::executable(
            dir.path(),
            "podman",
            &format!(
                "#!/bin/bash\necho \"podman $1\" >> '{calls}'\necho '{{}}'\n",
                calls = dir.path().join("calls").display()
            ),
        );
        Fixture {
            dir,
            store: Arc::new(Store::open_in_memory().unwrap()),
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn executor(&self) -> PodmanExecutor {
        let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(
            &[1; MASTER_KEY_LEN],
            Arc::clone(&self.store),
        ));
        PodmanExecutor {
            base: WorkerConfig {
                podman: self.dir.path().join("podman"),
                ..WorkerConfig::default()
            },
            git: Git {
                git: self.dir.path().join("git"),
            },
            forges: Forges::new(
                Arc::new(GitHub::default()),
                Arc::new(GhRepos {
                    gh: PathBuf::from("gh"),
                }),
            ),
            data_dir: self.dir.path().join("data"),
            registry: Arc::new(AgentRegistry::new(
                Arc::new(ClaudeToken::new(Arc::clone(&secrets), None)),
                Arc::new(CodexLogin::new(Arc::clone(&secrets))),
                secrets,
            )),
            store: Arc::clone(&self.store),
            plan_usage: Arc::new(PlanUsageCache::new()),
            local_probe: Arc::new(HttpLocalServerProbe::new()),
        }
    }

    async fn run(&self, request: AgentRequest) -> (RunReport, Vec<String>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let report = self
            .executor()
            .run_agent(
                &Settings::default(),
                request,
                Box::new(move |line| {
                    if let LogLine::Other(text) = line {
                        sink.lock().unwrap().push(text);
                    }
                }),
                Box::new(|_| {}),
            )
            .await;
        let lines = lines.lock().unwrap().clone();
        (report, lines)
    }
}

fn request(
    url: &str,
    key: Option<&str>,
    key_from: LocalKeyOwner,
    timeout: Duration,
) -> AgentRequest {
    AgentRequest {
        job_id: 7,
        forge: ForgeKind::GitHub,
        account: AccountSource::fixed(Account {
            token: Some("project-token".into()),
            login: "factory-bot".into(),
            name: "factory-bot".into(),
            email: "1+factory-bot@users.noreply.github.com".into(),
        }),
        repo: "owner/name".into(),
        base: "main".into(),
        branch: None,
        existing: None,
        merge_base: false,
        keep_checkout: false,
        session_id: "0b6f5c1e-8d1a-4c3e-9a57-3c1f2a9d7e10".into(),
        agent_session: None,
        agent: Agent::Local,
        expect: Expect::Spec,
        prompt: "Write the spec.".into(),
        model: Some("gemma".into()),
        effort: None,
        mcp: None,
        packages: Vec::new(),
        local: Some(LocalRequest {
            target: LocalTarget {
                cli: Cli::Codex,
                base_url: url.into(),
                key: key.map(str::to_owned),
                load_timeout: timeout,
            },
            key_from,
            setting_cli: Cli::Codex,
            session: factory_core::Session::Planning,
        }),
    }
}

async fn run(request: AgentRequest) -> (String, Vec<String>) {
    let (report, lines) = Fixture::new().run(request).await;
    let Event::AgentFailed { reason } = report.event else {
        panic!("expected AgentFailed");
    };
    (reason, lines)
}

const SHORT: Duration = Duration::from_secs(2);

#[tokio::test]
async fn an_immediate_answer_starts_the_worker_and_logs_nothing() {
    let server = stub(Answer::Ok, Duration::ZERO).await;
    let fixture = Fixture::new();
    let (_, lines) = fixture
        .run(request(&server.url, None, LocalKeyOwner::None, SHORT))
        .await;

    assert_eq!(
        fixture.calls().last().map(String::as_str),
        Some("podman run")
    );
    assert!(lines.is_empty(), "{lines:?}");
    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, None);
    assert_eq!(
        seen[0].1,
        serde_json::json!({
            "model": "gemma",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        })
    );
}

#[tokio::test]
async fn a_slow_load_logs_the_wait_and_starts_the_worker_only_after_the_answer() {
    let server = stub(Answer::Ok, Duration::from_secs(8)).await;
    let started = std::time::Instant::now();
    let fixture = Fixture::new();
    let (_, lines) = fixture
        .run(request(
            &server.url,
            Some(KEY),
            LocalKeyOwner::Factory,
            Duration::from_secs(60),
        ))
        .await;

    assert!(started.elapsed() >= Duration::from_secs(8));
    assert_eq!(
        fixture.calls().last().map(String::as_str),
        Some("podman run")
    );
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "Waiting for the local server to load `gemma`.");
    assert!(
        lines[1].starts_with("The local server answered after ") && lines[1].contains(" seconds"),
        "{lines:?}"
    );
    assert_eq!(
        server.seen.lock().unwrap()[0].0.as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
}

#[tokio::test]
async fn a_server_that_never_answers_fails_the_run_before_any_container() {
    let server = stub(Answer::Never, Duration::ZERO).await;
    let (reason, _) = run(request(
        &server.url,
        None,
        LocalKeyOwner::None,
        Duration::from_secs(1),
    ))
    .await;

    assert!(
        reason.starts_with("The local server did not answer for `gemma` within "),
        "{reason}"
    );
}

#[tokio::test]
async fn an_unreachable_server_fails_the_run() {
    let url = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (reason, _) = run(request(&url, None, LocalKeyOwner::None, SHORT)).await;

    assert_eq!(
        reason,
        format!("The factory cannot reach the local server at {url}.")
    );
}

#[tokio::test]
async fn a_401_names_whose_key_the_server_rejected_and_never_shows_a_key() {
    let denied = Answer::Status(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"message":"no"}}"#.into(),
    );
    let server = stub(denied.clone(), Duration::ZERO).await;

    let (reason, _) = run(request(&server.url, None, LocalKeyOwner::None, SHORT)).await;
    assert!(
        reason.starts_with("The local server asks for an API key and none is set."),
        "{reason}"
    );
    assert!(reason.contains("/settings/agents/local"), "{reason}");

    let (reason, _) = run(request(
        &server.url,
        Some(KEY),
        LocalKeyOwner::Login("alice".into()),
        SHORT,
    ))
    .await;
    assert!(
        reason.starts_with("The local server rejected the Local API key of alice."),
        "{reason}"
    );
    assert!(reason.contains("/settings/local"), "{reason}");

    let (reason, _) = run(request(
        &server.url,
        Some(KEY),
        LocalKeyOwner::Factory,
        SHORT,
    ))
    .await;
    assert!(
        reason.starts_with("The local server rejected the factory's Local API key."),
        "{reason}"
    );
    assert!(!reason.contains(KEY));

    let seen = server.seen.lock().unwrap();
    let bearers: Vec<_> = seen.iter().map(|(auth, _)| auth.clone()).collect();
    assert_eq!(
        bearers,
        [
            None,
            Some(format!("Bearer {KEY}")),
            Some(format!("Bearer {KEY}"))
        ]
    );
}

#[tokio::test]
async fn another_status_gives_the_status_and_the_servers_message() {
    let server = stub(
        Answer::Status(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"model 'gemma' not found"}}"#.into(),
        ),
        Duration::ZERO,
    )
    .await;
    let (reason, _) = run(request(&server.url, None, LocalKeyOwner::None, SHORT)).await;

    assert_eq!(
        reason,
        "The local server answered HTTP 404: model 'gemma' not found"
    );
}

#[tokio::test]
async fn a_pending_load_holds_back_the_checkout_and_the_worker_until_a_2xx() {
    let gate = Arc::new(Notify::new());
    let server = stub(Answer::Gated(Arc::clone(&gate)), Duration::ZERO).await;
    let fixture = Arc::new(Fixture::new());
    let run = tokio::spawn({
        let fixture = Arc::clone(&fixture);
        let request = request(
            &server.url,
            None,
            LocalKeyOwner::None,
            Duration::from_secs(60),
        );
        async move { fixture.run(request).await }
    });

    server.arrived.notified().await;
    assert!(!run.is_finished());
    assert!(!fixture.calls().contains(&"podman run".to_owned()));

    gate.notify_one();
    run.await.unwrap();
    assert_eq!(
        fixture.calls().last().map(String::as_str),
        Some("podman run")
    );
}

#[tokio::test]
async fn a_timed_out_load_starts_no_worker_and_the_job_offers_retry() {
    let server = stub(Answer::Never, Duration::ZERO).await;
    let fixture = Fixture::new();
    let (report, lines) = fixture
        .run(request(
            &server.url,
            None,
            LocalKeyOwner::None,
            Duration::from_secs(1),
        ))
        .await;

    assert_eq!(fixture.calls(), Vec::<String>::new());
    assert!(lines.is_empty(), "{lines:?}");
    let id = fixture.store.create("maestro", "alice", "x").unwrap();
    fixture.store.apply(id, Event::Start).unwrap();
    let job = fixture.store.apply(id, report.event).unwrap().job;
    assert!(job.can_retry());
}

#[tokio::test]
async fn a_late_error_answer_logs_the_seconds_waited() {
    let server = stub(
        Answer::Status(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"model 'gemma' not found"}}"#.into(),
        ),
        Duration::from_secs(6),
    )
    .await;
    let (_, lines) = run(request(
        &server.url,
        None,
        LocalKeyOwner::None,
        Duration::from_secs(60),
    ))
    .await;

    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "Waiting for the local server to load `gemma`.");
    assert!(
        lines[1].starts_with("The local server answered after "),
        "{lines:?}"
    );
}

#[tokio::test]
async fn a_key_the_server_echoes_across_the_cut_leaves_no_prefix() {
    let message = format!("{}{KEY}", "x".repeat(295));
    let server = stub(
        Answer::Status(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": {"message": message}}).to_string(),
        ),
        Duration::ZERO,
    )
    .await;
    let (reason, _) = run(request(
        &server.url,
        Some(KEY),
        LocalKeyOwner::Factory,
        SHORT,
    ))
    .await;

    assert!(
        reason.starts_with("The local server answered HTTP 400: xxx"),
        "{reason}"
    );
    assert!(!reason.contains("sk-"), "{reason}");
}

#[tokio::test]
async fn a_rejection_with_a_stalled_body_fails_at_once() {
    let server = stub(Answer::StalledBody(StatusCode::FORBIDDEN), Duration::ZERO).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run(request(
            &server.url,
            Some(KEY),
            LocalKeyOwner::Factory,
            Duration::from_secs(60),
        )),
    )
    .await
    .expect("the run waited for the body");

    assert!(
        outcome
            .0
            .starts_with("The local server rejected the factory's Local API key."),
        "{}",
        outcome.0
    );
}

#[tokio::test]
async fn an_error_with_a_stalled_body_keeps_its_status_and_does_not_wait_for_the_timeout() {
    let server = stub(Answer::StalledError(StatusCode::NOT_FOUND), Duration::ZERO).await;
    let (reason, lines) = tokio::time::timeout(
        Duration::from_secs(10),
        run(request(
            &server.url,
            None,
            LocalKeyOwner::None,
            Duration::from_secs(60),
        )),
    )
    .await
    .expect("the run waited for the body");

    assert_eq!(reason, "The local server answered HTTP 404.");
    assert!(lines.is_empty(), "{lines:?}");
}
