//! Tests for the Codex settings page: the stored-login state and `last_refresh`
//! with no secret in the HTML, the write-only paste field and Clear, the
//! disabled state when the store is unavailable, and the device login flow
//! driven by a fake login container. No podman runs: the fake prints a URL and
//! a code, then writes `auth.json` or exits in failure.

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
    MASTER_KEY_LEN, McpChecker, PlanUsageCache, Projects, RepoChecker, RepoReport, SecretStore,
    Store, USER_HEADER, UnavailableSecrets, WorkerSlots, app,
};
use http_body_util::BodyExt;
use tokio::sync::Notify;
use tower::ServiceExt;

/// A valid subscription `auth.json` with the given `last_refresh`.
fn auth(last_refresh: &str) -> String {
    format!(
        r#"{{"tokens":{{"access_token":"access-secret-token-value-aaaaaaaa","refresh_token":"refresh-secret-bbbbbbbb"}},"last_refresh":"{last_refresh}"}}"#
    )
}

/// What the fake container does once its output is closed.
#[derive(Clone)]
enum Outcome {
    /// Writes this `auth.json` into `CODEX_HOME` and exits with success.
    Success(String),
    /// Exits with failure and writes no file.
    Failure,
}

/// The script one login runs: the lines it prints, then its outcome.
#[derive(Clone)]
struct Script {
    lines: Vec<String>,
    outcome: Outcome,
}

/// A login spawner the test configures with a script. It records the containers
/// it spawned and killed, and holds the signal that closes the running
/// container's output.
#[derive(Default)]
struct FakeSpawner {
    script: Mutex<Option<Script>>,
    spawned: Mutex<Vec<String>>,
    homes: Mutex<Vec<PathBuf>>,
    killed: Arc<Mutex<Vec<String>>>,
    finish: Arc<Notify>,
}

impl FakeSpawner {
    fn set(&self, script: Script) {
        *self.script.lock().unwrap() = Some(script);
    }

    /// Closes the running container's output, so the flow reads back `auth.json`
    /// and resolves to signed-in or failed.
    fn finish(&self) {
        self.finish.notify_one();
    }

    fn killed(&self) -> Vec<String> {
        self.killed.lock().unwrap().clone()
    }

    fn spawned(&self) -> Vec<String> {
        self.spawned.lock().unwrap().clone()
    }

    /// The `CODEX_HOME` host directory each login was started with.
    fn homes(&self) -> Vec<PathBuf> {
        self.homes.lock().unwrap().clone()
    }
}

impl LoginSpawner for FakeSpawner {
    fn spawn(
        &self,
        name: String,
        _image: String,
        codex_home: PathBuf,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Box<dyn LoginProcess>>> + Send + '_>> {
        self.spawned.lock().unwrap().push(name.clone());
        self.homes.lock().unwrap().push(codex_home.clone());
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
                codex_home,
                finish,
                killed,
            }) as Box<dyn LoginProcess>)
        })
    }
}

/// A running fake login container. It prints its scripted lines, then blocks
/// until the test closes it, then applies its outcome.
struct FakeProcess {
    name: String,
    lines: VecDeque<String>,
    outcome: Outcome,
    codex_home: PathBuf,
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
            if let Outcome::Success(auth) = &self.outcome {
                std::fs::write(self.codex_home.join("auth.json"), auth).unwrap();
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
struct NoRepos;

impl RepoChecker for NoRepos {
    fn check<'a>(
        &'a self,
        _repo: &'a str,
        _base: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RepoReport, String>> + Send + 'a>> {
        Box::pin(async { Err("no repository checks in these tests".to_owned()) })
    }
}

/// An MCP checker the Codex pages never reach.
struct NoChecker;

impl McpChecker for NoChecker {
    fn check<'a>(
        &'a self,
        _url: &'a str,
        _headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = factory_server::CheckOutcome> + Send + 'a>> {
        Box::pin(async { factory_server::CheckOutcome::Unreachable })
    }
}

struct Harness {
    router: Router,
    store: Arc<Store>,
    secrets: Arc<dyn SecretStore>,
    spawner: Arc<FakeSpawner>,
    data: tempfile::TempDir,
}

/// The login timeout for the tests that are not about the timeout. It is long
/// enough that a slow poll never ends a login early.
const LONG_TIMEOUT: Duration = Duration::from_secs(60);

fn harness() -> Harness {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[7; MASTER_KEY_LEN], Arc::clone(&store)));
    harness_with(&store, secrets, LONG_TIMEOUT)
}

/// A harness whose store is unavailable, so the login button and the paste
/// field are disabled.
fn harness_unavailable() -> Harness {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let secrets: Arc<dyn SecretStore> = Arc::new(UnavailableSecrets::new(Arc::clone(&store)));
    harness_with(&store, secrets, LONG_TIMEOUT)
}

/// A harness whose login times out quickly, for the timeout test.
fn harness_fast_timeout() -> Harness {
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
    ));
    let device_login = Arc::new(DeviceLogin::with_timeout(
        Arc::clone(&secrets),
        Arc::clone(&spawner) as Arc<dyn LoginSpawner>,
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
        device_login,
    );
    Harness {
        router,
        store: Arc::clone(store),
        secrets,
        spawner,
        data,
    }
}

struct Reply {
    status: StatusCode,
    body: String,
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
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

    /// Polls the login fragment until `want` appears in it, or fails after a
    /// bounded wait. It returns the matching body so a test can make more
    /// assertions on it.
    async fn poll_fragment(&self, want: &str) -> String {
        for _ in 0..200 {
            let reply = self.get("/settings/agents/codex/login/fragment").await;
            if reply.body.contains(want) {
                return reply.body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the login fragment never showed {want:?}");
    }
}

#[tokio::test]
async fn the_page_never_shows_the_stored_login() {
    let harness = harness();
    let login = auth("2026-11-20T00:00:00Z");
    harness.secrets.set_codex_auth(&login).unwrap();

    let page = harness.get("/settings/agents/codex").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("access-secret-token-value"));
    assert!(!page.body.contains("refresh-secret"));
    // The last_refresh time is shown, the token is not.
    assert!(page.body.contains("2026-11-20"));
    assert!(page.body.contains("A login is stored"));
}

#[tokio::test]
async fn a_valid_paste_is_stored_and_still_absent_from_the_html() {
    let harness = harness();
    let login = auth("2026-11-20T00:00:00Z");
    let form = format!("auth={}", urlencoding(&login));

    let reply = harness.post("/settings/agents/codex/auth", &form).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Saved."));
    assert!(!reply.body.contains("access-secret-token-value"));
    assert_eq!(
        harness.secrets.codex_auth().unwrap().as_deref(),
        Some(login.as_str())
    );
}

#[tokio::test]
async fn an_invalid_paste_is_refused_and_stores_nothing() {
    let harness = harness();
    let reply = harness
        .post(
            "/settings/agents/codex/auth",
            "auth=%7B%22OPENAI_API_KEY%22%3A%22sk%22%7D",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("tokens.access_token"));
    assert!(harness.secrets.codex_auth().unwrap().is_none());
}

#[tokio::test]
async fn an_empty_save_keeps_the_stored_login() {
    let harness = harness();
    let login = auth("2026-11-20T00:00:00Z");
    harness.secrets.set_codex_auth(&login).unwrap();

    let reply = harness.post("/settings/agents/codex/auth", "auth=").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        harness.secrets.codex_auth().unwrap().as_deref(),
        Some(login.as_str())
    );
}

#[tokio::test]
async fn clear_removes_the_stored_login() {
    let harness = harness();
    harness
        .secrets
        .set_codex_auth(&auth("2026-11-20T00:00:00Z"))
        .unwrap();

    let reply = harness.post("/settings/agents/codex/auth/clear", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Cleared."));
    assert!(harness.secrets.codex_auth().unwrap().is_none());
}

#[tokio::test]
async fn without_a_master_key_the_controls_are_disabled_and_clear_works() {
    let harness = harness_unavailable();
    let page = harness.get("/settings/agents/codex").await;
    assert_eq!(page.status, StatusCode::OK);
    // The paste field and the sign-in button are disabled.
    assert!(page.body.contains("disabled"));
    assert!(page.body.contains("master key is missing"));

    // Clear still answers without error.
    let reply = harness.post("/settings/agents/codex/auth/clear", "").await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn device_login_shows_the_url_and_code_then_signs_in() {
    let harness = harness();
    let login = auth("2026-11-28T00:00:00Z");
    harness.spawner.set(Script {
        lines: vec![
            "To sign in, open https://auth.openai.com/device and enter the code".to_owned(),
            "Code: WDJB-MJHT".to_owned(),
        ],
        outcome: Outcome::Success(login.clone()),
    });

    let start = harness.post("/settings/agents/codex/login", "").await;
    assert_eq!(start.status, StatusCode::OK);

    let waiting = harness.poll_fragment("WDJB-MJHT").await;
    assert!(waiting.contains("https://auth.openai.com/device"));
    assert_eq!(harness.spawner.spawned().len(), 1);

    // The operator finished in the browser, so the container writes auth.json
    // and exits.
    harness.spawner.finish();
    harness.poll_fragment("Signed in").await;
    assert_eq!(
        harness.secrets.codex_auth().unwrap().as_deref(),
        Some(login.as_str())
    );
}

/// The factory runs in a container and starts the login through the host's
/// podman socket, so the `CODEX_HOME` it mounts must be a path the host has:
/// one under the data directory, not the container's `/tmp`.
#[tokio::test]
async fn the_login_home_is_made_under_the_data_directory() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: vec!["Open https://auth.openai.com/device and enter WDJB-MJHT".to_owned()],
        outcome: Outcome::Success(auth("2026-11-28T00:00:00Z")),
    });

    harness.post("/settings/agents/codex/login", "").await;
    harness.poll_fragment("WDJB-MJHT").await;

    let homes = harness.spawner.homes();
    assert_eq!(homes.len(), 1);
    assert!(
        homes[0].starts_with(harness.data.path().join("codex-login")),
        "{} is not under the data directory",
        homes[0].display()
    );
    assert!(homes[0].join("config.toml").is_file());

    // The run task deletes the directory once the login ends.
    harness.spawner.finish();
    harness.poll_fragment("Signed in").await;
    assert!(!homes[0].exists());
}

#[tokio::test]
async fn a_failed_device_login_shows_a_reason_and_stores_nothing() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: vec!["waiting for sign-in".to_owned()],
        outcome: Outcome::Failure,
    });

    harness.post("/settings/agents/codex/login", "").await;
    // The scripted line is read, then the container closes in failure.
    harness.spawner.finish();

    let failed = harness.poll_fragment("did not finish").await;
    assert!(failed.contains("waiting for sign-in"));
    assert!(harness.secrets.codex_auth().unwrap().is_none());
}

#[tokio::test]
async fn a_secret_in_the_failure_output_is_scrubbed() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: vec!["error eyJhbGciOiJIUzI1NiwicmVmcmVzaCI6InNlY3JldC12YWx1ZSJ9".to_owned()],
        outcome: Outcome::Failure,
    });

    harness.post("/settings/agents/codex/login", "").await;
    harness.spawner.finish();
    let failed = harness.poll_fragment("did not finish").await;
    assert!(failed.contains("[redacted]"));
    assert!(!failed.contains("eyJhbGci"));
}

#[tokio::test]
async fn cancel_removes_the_container() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: vec![
            "Open https://auth.openai.com/device".to_owned(),
            "Code: WDJB-MJHT".to_owned(),
        ],
        outcome: Outcome::Success(auth("2026-11-28T00:00:00Z")),
    });

    harness.post("/settings/agents/codex/login", "").await;
    harness.poll_fragment("WDJB-MJHT").await;

    let reply = harness
        .post("/settings/agents/codex/login/cancel", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    for _ in 0..200 {
        if harness.spawner.killed() == vec!["factory-codex-login".to_owned()] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        harness.spawner.killed(),
        vec!["factory-codex-login".to_owned()]
    );
    // No login was stored.
    assert!(harness.secrets.codex_auth().unwrap().is_none());
}

#[tokio::test]
async fn only_one_login_runs_at_a_time() {
    let harness = harness();
    harness.spawner.set(Script {
        lines: vec![
            "Open https://auth.openai.com/device".to_owned(),
            "Code: WDJB-MJHT".to_owned(),
        ],
        outcome: Outcome::Success(auth("2026-11-28T00:00:00Z")),
    });

    harness.post("/settings/agents/codex/login", "").await;
    harness.poll_fragment("WDJB-MJHT").await;
    // A second start while the first waits does not spawn another container.
    harness.post("/settings/agents/codex/login", "").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.spawner.spawned().len(), 1);
}

#[tokio::test]
async fn a_login_that_never_finishes_times_out() {
    let harness = harness_fast_timeout();
    harness.spawner.set(Script {
        lines: vec![
            "Open https://auth.openai.com/device".to_owned(),
            "Code: WDJB-MJHT".to_owned(),
        ],
        outcome: Outcome::Success(auth("2026-11-28T00:00:00Z")),
    });

    harness.post("/settings/agents/codex/login", "").await;
    // The 200 ms timeout ends the login with no close signal from the test.
    let failed = harness.poll_fragment("did not finish").await;
    assert!(failed.contains("fifteen minutes"));
    assert!(harness.secrets.codex_auth().unwrap().is_none());
}

#[tokio::test]
async fn the_price_table_starts_empty() {
    let harness = harness();
    let page = harness.get("/settings/agents/codex").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Model prices"));
    assert!(page.body.contains("No model prices are set"));
    assert_eq!(harness.store.codex_prices().unwrap(), []);
}

#[tokio::test]
async fn a_price_is_added_edited_and_deleted() {
    let harness = harness();

    // Add a model price.
    let add = harness
        .post(
            "/settings/agents/codex/prices",
            "model=gpt-6.1-sol&input=1.25&cached_input=0.125&output=10",
        )
        .await;
    assert_eq!(add.status, StatusCode::OK);
    assert!(add.body.contains("Saved."));
    assert!(add.body.contains("gpt-6.1-sol"));
    let stored = harness.store.codex_price("gpt-6.1-sol").unwrap().unwrap();
    assert!((stored.input_usd_per_mtok - 1.25).abs() < 1e-9);
    assert!((stored.cached_input_usd_per_mtok - 0.125).abs() < 1e-9);
    assert!((stored.output_usd_per_mtok - 10.0).abs() < 1e-9);

    // Saving the same model again edits the row rather than adding a second.
    let edit = harness
        .post(
            "/settings/agents/codex/prices",
            "model=gpt-6.1-sol&input=2&cached_input=0.2&output=12",
        )
        .await;
    assert_eq!(edit.status, StatusCode::OK);
    assert_eq!(harness.store.codex_prices().unwrap().len(), 1);
    let edited = harness.store.codex_price("gpt-6.1-sol").unwrap().unwrap();
    assert!((edited.input_usd_per_mtok - 2.0).abs() < 1e-9);

    // Delete it.
    let delete = harness
        .post("/settings/agents/codex/prices/delete", "model=gpt-6.1-sol")
        .await;
    assert_eq!(delete.status, StatusCode::OK);
    assert!(delete.body.contains("Deleted."));
    assert!(harness.store.codex_price("gpt-6.1-sol").unwrap().is_none());
}

#[tokio::test]
async fn an_invalid_price_is_refused_and_stores_nothing() {
    let harness = harness();

    // A blank model is refused.
    let blank = harness
        .post(
            "/settings/agents/codex/prices",
            "model=&input=1&cached_input=1&output=1",
        )
        .await;
    assert_eq!(blank.status, StatusCode::OK);
    assert!(blank.body.contains("must not be empty"));

    // A non-numeric price is refused and names the field.
    let bad = harness
        .post(
            "/settings/agents/codex/prices",
            "model=gpt-6.1-sol&input=cheap&cached_input=1&output=1",
        )
        .await;
    assert_eq!(bad.status, StatusCode::OK);
    assert!(bad.body.contains("input price"));
    assert_eq!(harness.store.codex_prices().unwrap(), []);

    // A negative price is refused too.
    let negative = harness
        .post(
            "/settings/agents/codex/prices",
            "model=gpt-6.1-sol&input=1&cached_input=1&output=-1",
        )
        .await;
    assert_eq!(negative.status, StatusCode::OK);
    assert!(negative.body.contains("output price"));
    assert_eq!(harness.store.codex_prices().unwrap(), []);
}

/// Percent-encodes a form value the way the browser posts the paste field.
fn urlencoding(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}
