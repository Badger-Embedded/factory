//! A fake login container and an app wired to it, shared by the Copilot
//! settings and login tests.

#![allow(dead_code, reason = "each test binary uses a part of the harness")]

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use factory_server::{
    AgentRegistry, ClaudeToken, CodexLogin, DeviceLogin, LocalSecrets, LoginProcess, LoginSpawner,
    LoginSpec, MASTER_KEY_LEN, McpChecker, PlanUsageCache, Projects, RepoChecker, RepoReport,
    SecretStore, Store, USER_HEADER, UnavailableSecrets, WorkerSlots, app,
};
use http_body_util::BodyExt;
use tokio::sync::Notify;
use tower::ServiceExt;

/// The `config.json` a Copilot login writes: the same token under two
/// `authTokens` entries, as the pinned CLI does.
pub fn config_json(token: &str) -> String {
    format!(
        r#"{{"authTokens":{{"https://github.com:alice:github":{{"token":"{token}"}},"https://github.com:alice":{{"token":"{token}"}}}},"loggedInUsers":[],"lastLoggedInUser":null}}"#
    )
}

/// What the fake container does once its output is closed.
#[derive(Clone)]
pub enum Outcome {
    /// Writes this `config.json` into `COPILOT_HOME` and exits with success.
    Success(String),
    /// Exits with failure and writes no file.
    Failure,
}

/// The script one login runs: the lines it prints, then its outcome.
#[derive(Clone)]
pub struct Script {
    pub lines: Vec<String>,
    pub outcome: Outcome,
}

/// A login spawner the test configures with a script. It records the containers
/// it spawned and killed, and holds the signal that closes the running
/// container's output.
#[derive(Default)]
pub struct FakeSpawner {
    script: Mutex<Option<Script>>,
    spawned: Mutex<Vec<String>>,
    homes: Mutex<Vec<PathBuf>>,
    killed: Arc<Mutex<Vec<String>>>,
    finish: Arc<Notify>,
}

impl FakeSpawner {
    pub fn set(&self, script: Script) {
        *self.script.lock().unwrap() = Some(script);
    }

    /// Closes the running container's output, so the flow reads back `config.json`
    /// and resolves to signed-in or failed.
    pub fn finish(&self) {
        self.finish.notify_one();
    }

    pub fn killed(&self) -> Vec<String> {
        self.killed.lock().unwrap().clone()
    }

    pub fn spawned(&self) -> Vec<String> {
        self.spawned.lock().unwrap().clone()
    }

    /// The `COPILOT_HOME` host directory each login was started with.
    pub fn homes(&self) -> Vec<PathBuf> {
        self.homes.lock().unwrap().clone()
    }
}

impl LoginSpawner for FakeSpawner {
    fn spawn(
        &self,
        name: String,
        _image: String,
        copilot_home: PathBuf,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Box<dyn LoginProcess>>> + Send + '_>> {
        self.spawned.lock().unwrap().push(name.clone());
        self.homes.lock().unwrap().push(copilot_home.clone());
        let script = self.script.lock().unwrap().clone();
        let finish = Arc::clone(&self.finish);
        let killed = Arc::clone(&self.killed);
        Box::pin(async move {
            let Some(script) = script else {
                return Err(std::io::Error::other("no script set for the fake login"));
            };
            Ok(Box::new(FakeProcess {
                name,
                lines: script.lines.into_iter().collect(),
                outcome: script.outcome,
                copilot_home,
                finish,
                killed,
            }) as Box<dyn LoginProcess>)
        })
    }
}

/// A running fake login container. It prints its scripted lines, then blocks
/// until the test closes it, then applies its outcome.
pub struct FakeProcess {
    name: String,
    lines: VecDeque<String>,
    outcome: Outcome,
    copilot_home: PathBuf,
    finish: Arc<Notify>,
    killed: Arc<Mutex<Vec<String>>>,
}

impl LoginProcess for FakeProcess {
    fn next_line(&mut self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(async move {
            if let Some(line) = self.lines.pop_front() {
                return Some(line);
            }
            self.finish.notified().await;
            if let Outcome::Success(config) = &self.outcome {
                std::fs::write(self.copilot_home.join("config.json"), config).unwrap();
            }
            None
        })
    }

    fn wait(&mut self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move { matches!(self.outcome, Outcome::Success(_)) })
    }

    fn kill(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.killed.lock().unwrap().push(self.name.clone());
        // Unblock a next_line waiting on the close signal, so the task ends.
        self.finish.notify_one();
        Box::pin(std::future::ready(()))
    }
}

/// A repository checker the onboarding wizard never reaches in these tests.
pub struct NoRepos;

impl RepoChecker for NoRepos {
    fn check<'a>(
        &'a self,
        _repo: &'a str,
        _base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        Box::pin(async { Err("no repository checks in these tests".to_owned()) })
    }
}

/// An MCP checker the Copilot pages never reach.
pub struct NoChecker;

impl McpChecker for NoChecker {
    fn check<'a>(
        &'a self,
        _url: &'a str,
        _headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = factory_server::CheckOutcome> + Send + 'a>> {
        Box::pin(async { factory_server::CheckOutcome::Unreachable })
    }
}

pub struct Harness {
    pub router: Router,
    pub store: Arc<Store>,
    pub secrets: Arc<dyn SecretStore>,
    pub spawner: Arc<FakeSpawner>,
    pub data: tempfile::TempDir,
}

/// The login timeout for the tests that are not about the timeout. It is long
/// enough that a slow poll never ends a login early.
pub const LONG_TIMEOUT: Duration = Duration::from_secs(60);

pub fn harness() -> Harness {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    harness_with(&store, secrets, LONG_TIMEOUT)
}

/// A harness whose store is unavailable, so the login button and the paste
/// field are disabled.
pub fn harness_unavailable() -> Harness {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let secrets: Arc<dyn SecretStore> = Arc::new(UnavailableSecrets::new(Arc::clone(&store)));
    harness_with(&store, secrets, LONG_TIMEOUT)
}

/// A harness whose login times out quickly, for the timeout test.
pub fn harness_fast_timeout() -> Harness {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    harness_with(&store, secrets, Duration::from_millis(150))
}

fn harness_with(store: &Arc<Store>, secrets: Arc<dyn SecretStore>, timeout: Duration) -> Harness {
    let data = tempfile::tempdir().unwrap();
    let spawner = Arc::new(FakeSpawner::default());
    let registry = Arc::new(AgentRegistry::new(
        Arc::new(ClaudeToken::new(Arc::clone(&secrets), None)),
        Arc::new(CodexLogin::new(Arc::clone(&secrets))),
        Arc::clone(&secrets),
    ));
    let codex_login = Arc::new(DeviceLogin::with_timeout(
        Arc::clone(&secrets),
        Arc::new(FakeSpawner::default()) as Arc<dyn LoginSpawner>,
        data.path().join("codex-login"),
        timeout,
    ));
    let router = app(
        Arc::clone(store),
        Arc::new(Projects::from_store(Arc::clone(store))),
        Arc::new(Notify::new()),
        data.path().to_owned(),
        Arc::new(PlanUsageCache::new()),
        Arc::new(NoRepos) as Arc<dyn RepoChecker>,
        Arc::new(WorkerSlots::new()),
        Arc::clone(&secrets),
        Arc::new(NoChecker) as Arc<dyn McpChecker>,
        registry,
        codex_login,
        Arc::new(DeviceLogin::for_spec_with_timeout(
            LoginSpec::COPILOT,
            Arc::clone(&secrets),
            Arc::clone(&spawner) as Arc<dyn LoginSpawner>,
            data.path().join("copilot-login"),
            timeout,
        )),
    );
    Harness {
        router,
        store: Arc::clone(store),
        secrets,
        spawner,
        data,
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub body: String,
    pub headers: axum::http::HeaderMap,
}

impl Harness {
    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            headers,
        }
    }

    pub async fn get(&self, uri: &str) -> Reply {
        self.send(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    pub async fn post(&self, uri: &str, form: &str) -> Reply {
        self.send(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(USER_HEADER, "alice@example.com")
                .body(Body::from(form.to_owned()))
                .unwrap(),
        )
        .await
    }

    /// Polls the login fragment until `want` appears in it, or fails after a
    /// bounded wait. It returns the matching body so a test can make more
    /// assertions on it.
    pub async fn poll_fragment(&self, want: &str) -> String {
        for _ in 0..200 {
            let reply = self.get("/settings/agents/copilot/login/fragment").await;
            if reply.body.contains(want) {
                return reply.body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the login fragment never showed {want:?}");
    }
}
