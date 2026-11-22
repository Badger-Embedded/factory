//! The device-login flow on an agent's settings page, such as "Sign in with
//! `ChatGPT`" for Codex. A [`LoginSpec`] holds what differs per agent.
//!
//! A `ChatGPT` Plus or Pro subscription has no pasteable access token, so the
//! only way into the factory is the `auth.json` a `codex login` writes. This
//! runs that login on moon, where no browser is present, with
//! `codex login --device-auth`: the factory starts a worker-image container
//! that prints a verification URL and a one-time code, the operator opens the
//! URL and enters the code on another machine, and Codex writes `auth.json`
//! into the container's `CODEX_HOME` and exits. The factory reads that file
//! back and stores it.
//!
//! [`DeviceLogin`] owns the one login a factory runs at a time. A settings
//! handler calls [`DeviceLogin::start`], an htmx fragment polls
//! [`DeviceLogin::snapshot`] every five seconds for the URL, the code, success
//! or a failure reason, and a Cancel button calls [`DeviceLogin::cancel`]. The
//! container runs behind the [`LoginSpawner`] trait, so a test drives the flow
//! with a fake process and no podman.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use factory_runner::{WorkerConfig, remove_container};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::secrets::{SecretError, SecretStore, validate_codex_auth};

/// What one agent's device login needs: the container it runs, the command, the
/// config directory it writes the credential into, and how the factory reads
/// that credential back, checks it and stores it. [`DeviceLogin`] and
/// [`PodmanLogin`] hold no agent-specific values of their own; an agent adds a
/// spec like [`LoginSpec::CODEX`].
#[derive(Clone, Copy)]
pub struct LoginSpec {
    /// The container the login runs in. One login runs at a time, so the name
    /// is fixed and Cancel can remove it.
    pub container_name: &'static str,
    /// The prefix of the fresh directory made under the scratch directory for
    /// each login.
    pub scratch_prefix: &'static str,
    /// The command run in the worker image.
    pub command: &'static [&'static str],
    /// The environment variable that points the agent at its config directory.
    pub home_env: &'static str,
    /// Where the config directory is mounted inside the container.
    pub home_mount: &'static str,
    /// Writes the config files the login needs into the fresh config directory.
    pub prepare: fn(home: &Path) -> std::io::Result<()>,
    /// Reads the credential the container wrote into the config directory, or
    /// `None` when there is none.
    pub read_back: fn(home: &Path) -> Option<String>,
    /// Whether the credential read back is one the factory accepts.
    pub validate: fn(credential: &str) -> bool,
    /// Stores an accepted credential in the secret store.
    pub store: fn(secrets: &dyn SecretStore, credential: &str) -> Result<(), SecretError>,
    /// The reason shown when the operator does not finish within the timeout.
    pub timeout_reason: &'static str,
}

impl LoginSpec {
    /// `codex login --device-auth`, which writes `auth.json`.
    pub const CODEX: LoginSpec = LoginSpec {
        container_name: "factory-codex-login",
        scratch_prefix: "codex-login-",
        command: &["codex", "login", "--device-auth"],
        home_env: "CODEX_HOME",
        home_mount: "/codex",
        prepare: codex_prepare,
        read_back: codex_read_back,
        validate: |auth| validate_codex_auth(auth).is_ok(),
        store: |secrets, auth| secrets.set_codex_auth(auth),
        timeout_reason: "It was not finished within fifteen minutes. Start it again, or \
                         paste a login below.",
    };
}

/// Writes the `config.toml` that makes Codex keep its credentials in
/// `auth.json` rather than an OS keyring the container has no access to.
fn codex_prepare(home: &Path) -> std::io::Result<()> {
    std::fs::write(
        home.join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )
}

fn codex_read_back(home: &Path) -> Option<String> {
    std::fs::read_to_string(home.join("auth.json")).ok()
}

/// The login ends in failure when the operator does not finish it within this.
const LOGIN_TIMEOUT: Duration = Duration::from_mins(15);

/// Output lines kept for a failure reason.
const RECENT_LINES: usize = 6;

/// A boxed future, as the [`LoginSpawner`] and [`LoginProcess`] trait methods
/// return.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Starts the login container and gives a handle to its output and lifetime. The podman implementation runs the real container; a
/// test supplies a fake that prints a URL and a code and then writes
/// `auth.json`, so the flow is exercised with no container.
pub trait LoginSpawner: Send + Sync {
    /// Starts the login named `name` from `image`, with the agent's config
    /// directory mounted from `codex_home` on the host, which already holds the
    /// files [`LoginSpec::prepare`] wrote. The container writes the credential
    /// into it on success.
    fn spawn(
        &self,
        name: String,
        image: String,
        codex_home: PathBuf,
    ) -> BoxFuture<'_, std::io::Result<Box<dyn LoginProcess>>>;
}

/// A running login container.
pub trait LoginProcess: Send {
    /// The next line the container printed on stdout or stderr, or `None` once
    /// both are closed, which means the container is exiting.
    fn next_line(&mut self) -> BoxFuture<'_, Option<String>>;

    /// Waits for the container to exit and reports whether it succeeded.
    fn wait(&mut self) -> BoxFuture<'_, bool>;

    /// Removes the container, so Cancel and the timeout end a login that is
    /// still waiting on the operator.
    fn kill(&mut self) -> BoxFuture<'_, ()>;
}

/// What the login fragment shows. [`DeviceLogin::snapshot`] returns it and the
/// page renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginView {
    /// No login is running. The page shows the "Sign in with `ChatGPT`" button.
    Idle,
    /// A login started but has not printed its URL and code yet.
    Starting,
    /// The operator must open `url` and enter `code`.
    Waiting { url: String, code: String },
    /// A login was stored.
    SignedIn,
    /// The login ended with no stored login, for `reason`.
    Failed { reason: String },
}

impl LoginView {
    /// Whether a login is running, so the page keeps polling and a second start
    /// shows this one rather than beginning another.
    #[must_use]
    pub fn active(&self) -> bool {
        matches!(self, LoginView::Starting | LoginView::Waiting { .. })
    }
}

/// The current login's view and the handle that cancels its task.
struct State {
    view: LoginView,
    /// Dropped or sent on to stop the running task; `None` once no task runs.
    cancel: Option<oneshot::Sender<()>>,
}

/// The one device login a factory runs at a time. Shared by the settings
/// handlers.
pub struct DeviceLogin {
    spec: LoginSpec,
    secrets: Arc<dyn SecretStore>,
    spawner: Arc<dyn LoginSpawner>,
    /// The directory each login's config directory is made in. The factory hands
    /// this path to podman, so it must have the same path on the host as in the
    /// factory container, as the data directory does; the container's `/tmp`
    /// does not exist on the host.
    scratch: PathBuf,
    timeout: Duration,
    state: Arc<Mutex<State>>,
}

impl DeviceLogin {
    /// Builds the coordinator from the secret store the login is saved to, the
    /// spawner that runs the container, and the directory each login's
    /// config directory is made in.
    #[must_use]
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        spawner: Arc<dyn LoginSpawner>,
        scratch: PathBuf,
    ) -> Self {
        Self::with_timeout(secrets, spawner, scratch, LOGIN_TIMEOUT)
    }

    /// Builds the coordinator for the agent `spec` describes, with the standard
    /// timeout.
    #[must_use]
    pub fn for_spec(
        spec: LoginSpec,
        secrets: Arc<dyn SecretStore>,
        spawner: Arc<dyn LoginSpawner>,
        scratch: PathBuf,
    ) -> Self {
        Self::for_spec_with_timeout(spec, secrets, spawner, scratch, LOGIN_TIMEOUT)
    }

    /// As [`DeviceLogin::new`], with the timeout given, so a test can end a
    /// stuck login without waiting fifteen minutes.
    #[must_use]
    pub fn with_timeout(
        secrets: Arc<dyn SecretStore>,
        spawner: Arc<dyn LoginSpawner>,
        scratch: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self::for_spec_with_timeout(LoginSpec::CODEX, secrets, spawner, scratch, timeout)
    }

    /// As [`DeviceLogin::for_spec`], with the timeout given.
    #[must_use]
    pub fn for_spec_with_timeout(
        spec: LoginSpec,
        secrets: Arc<dyn SecretStore>,
        spawner: Arc<dyn LoginSpawner>,
        scratch: PathBuf,
        timeout: Duration,
    ) -> Self {
        DeviceLogin {
            spec,
            secrets,
            spawner,
            scratch,
            timeout,
            state: Arc::new(Mutex::new(State {
                view: LoginView::Idle,
                cancel: None,
            })),
        }
    }

    /// The current state of the login, for the page and its polling fragment.
    pub async fn snapshot(&self) -> LoginView {
        self.state.lock().await.view.clone()
    }

    /// Starts a login in a container from `image`, unless one is already
    /// running, in which case the running one is left alone. A fresh directory
    /// under the scratch directory becomes the container's config directory; the
    /// container writes the credential there, and the run task reads it back and
    /// deletes the directory when it ends.
    pub async fn start(&self, image: String) {
        let mut state = self.state.lock().await;
        if state.view.active() {
            return;
        }
        let dir = std::fs::create_dir_all(&self.scratch).and_then(|()| {
            tempfile::Builder::new()
                .prefix(self.spec.scratch_prefix)
                .tempdir_in(&self.scratch)
        });
        let dir = match dir {
            Ok(dir) => dir,
            Err(err) => {
                state.view = LoginView::Failed {
                    reason: format!("Could not make a directory for the login: {err}"),
                };
                return;
            }
        };
        if let Err(err) = (self.spec.prepare)(dir.path()) {
            state.view = LoginView::Failed {
                reason: format!("Could not write the login config: {err}"),
            };
            return;
        }
        let (cancel_tx, cancel_rx) = oneshot::channel();
        state.view = LoginView::Starting;
        state.cancel = Some(cancel_tx);
        drop(state);

        let task = RunTask {
            spec: self.spec,
            secrets: Arc::clone(&self.secrets),
            spawner: Arc::clone(&self.spawner),
            state: Arc::clone(&self.state),
            timeout: self.timeout,
        };
        tokio::spawn(task.run(image, dir, cancel_rx));
    }

    /// Cancels a running login: the state goes back to idle at once and the
    /// task removes the container. A call with no login running does nothing.
    pub async fn cancel(&self) {
        let mut state = self.state.lock().await;
        if let Some(cancel) = state.cancel.take() {
            let _ = cancel.send(());
            state.view = LoginView::Idle;
        }
    }
}

/// One run of the login flow, spawned as a task by [`DeviceLogin::start`].
struct RunTask {
    spec: LoginSpec,
    secrets: Arc<dyn SecretStore>,
    spawner: Arc<dyn LoginSpawner>,
    state: Arc<Mutex<State>>,
    timeout: Duration,
}

impl RunTask {
    /// Runs the container to the end, publishing the URL and code as they
    /// arrive, then stores the credential the container wrote or reports why it could
    /// not. The container is removed on a cancel or the timeout. `dir` holds the
    /// container's config directory and is deleted when this returns.
    async fn run(self, image: String, dir: tempfile::TempDir, cancel: oneshot::Receiver<()>) {
        let home = dir.path().to_owned();
        let mut process = match self
            .spawner
            .spawn(self.spec.container_name.to_owned(), image, home.clone())
            .await
        {
            Ok(process) => process,
            Err(err) => {
                self.fail(format!("Could not start the login container: {err}"))
                    .await;
                return;
            }
        };

        let timeout = tokio::time::sleep(self.timeout);
        tokio::pin!(timeout);
        tokio::pin!(cancel);
        let mut seen = String::new();
        let mut recent: VecDeque<String> = VecDeque::with_capacity(RECENT_LINES);
        let mut found_prompt = false;
        loop {
            tokio::select! {
                line = process.next_line() => match line {
                    Some(line) => {
                        let line = strip_ansi(&line);
                        push_recent(&mut recent, &line);
                        if !found_prompt {
                            seen.push_str(&line);
                            seen.push('\n');
                            if let Some((url, code)) = parse_prompt(&seen) {
                                found_prompt = true;
                                self.publish(LoginView::Waiting { url, code }).await;
                            }
                        }
                    }
                    None => break,
                },
                _ = &mut cancel => {
                    // Cancel already set the view to idle; just remove the
                    // container so the login stops waiting on the operator.
                    process.kill().await;
                    return;
                }
                () = &mut timeout => {
                    process.kill().await;
                    self.fail(self.spec.timeout_reason.to_owned()).await;
                    return;
                }
            }
        }

        let succeeded = process.wait().await;
        self.finish(succeeded, &home, &recent).await;
    }

    /// Stores the `auth.json` the container wrote, or reports why the login did
    /// not produce one. A valid file is stored even when the container's exit
    /// code was non-zero, so a login that succeeded but exited oddly is kept;
    /// an invalid or missing file is a failure with the container's last output.
    async fn finish(&self, succeeded: bool, home: &Path, recent: &VecDeque<String>) {
        match (self.spec.read_back)(home) {
            Some(credential) if (self.spec.validate)(&credential) => {
                if let Err(err) = (self.spec.store)(self.secrets.as_ref(), &credential) {
                    self.fail(format!("The login could not be saved: {err}"))
                        .await;
                } else {
                    self.publish(LoginView::SignedIn).await;
                }
            }
            _ => {
                let reason = match scrub_lines(recent) {
                    Some(tail) => format!("The container's last output:\n{tail}"),
                    None if succeeded => {
                        "The container exited without writing a usable login.".to_owned()
                    }
                    None => "The container exited before signing in.".to_owned(),
                };
                self.fail(reason).await;
            }
        }
    }

    /// Publishes a non-terminal view with the cancel handle kept, or a terminal
    /// one with it cleared, so no stale handle outlives the task.
    async fn publish(&self, view: LoginView) {
        let mut state = self.state.lock().await;
        if !view.active() {
            state.cancel = None;
        }
        state.view = view;
    }

    /// Publishes a failure, clearing the cancel handle.
    async fn fail(&self, reason: String) {
        self.publish(LoginView::Failed { reason }).await;
    }
}

/// A line with its ANSI escape sequences removed. Codex colors the URL and the
/// code, as in `\x1b[94mhttps://auth.openai.com/codex/device\x1b[0m`, and the
/// color codes would otherwise stick to the tokens [`parse_prompt`] looks for.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // A CSI sequence, `ESC [` then parameters, ends at a byte in `@`..=`~`.
        // Any other escape drops the one character after `ESC`.
        if chars.next_if_eq(&'[').is_some() {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

/// Keeps the last [`RECENT_LINES`] output lines for a failure reason.
fn push_recent(recent: &mut VecDeque<String>, line: &str) {
    if recent.len() == RECENT_LINES {
        recent.pop_front();
    }
    recent.push_back(line.to_owned());
}

/// The verification URL and one-time code from the login's output, once both
/// have been printed. The URL is the first `https://` token and the code the
/// first token shaped like `WDJB-MJHT`, so the two are found whether Codex
/// prints them on one line or several.
fn parse_prompt(text: &str) -> Option<(String, String)> {
    let url = text
        .split_whitespace()
        .map(trim_token)
        .find(|token| token.starts_with("https://"))?;
    let code = text
        .split_whitespace()
        .map(trim_token)
        .find(|token| is_code(token))?;
    Some((url.to_owned(), code.to_owned()))
}

/// Whether a token is a device code such as `WDJB-MJHT`: two groups of three to
/// eight uppercase letters or digits around a single dash. Codex's prompt says
/// "Enter this one-time code", and the uppercase rule keeps `one-time` from
/// matching.
fn is_code(token: &str) -> bool {
    let Some((left, right)) = token.split_once('-') else {
        return false;
    };
    let group = |part: &str| {
        (3..=8).contains(&part.len())
            && part
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    group(left) && group(right)
}

/// A token with surrounding punctuation removed, so a URL or code in prose such
/// as `open https://host/device.` is read without its trailing period.
fn trim_token(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':' && c != '/' && c != '-')
}

/// The recent output lines, each scrubbed of token-like values, joined for a
/// failure reason, or `None` when there is nothing to show.
fn scrub_lines(recent: &VecDeque<String>) -> Option<String> {
    let scrubbed: Vec<String> = recent
        .iter()
        .map(|line| scrub_line(line))
        .filter(|line| !line.trim().is_empty())
        .collect();
    if scrubbed.is_empty() {
        None
    } else {
        Some(scrubbed.join("\n"))
    }
}

/// A line with every token-like value replaced by `[redacted]`, so an access
/// token or a JWT the container happened to print never reaches the page. A
/// device code is short and stays, so a failure that still shows the code
/// remains readable.
fn scrub_line(line: &str) -> String {
    line.split_whitespace()
        .map(|token| {
            if looks_secret(token) {
                "[redacted]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a token looks like a secret: a JWT or an API key by its prefix, or
/// any run of twenty or more characters from a token alphabet. The device code,
/// at most seventeen characters, is below the length cut.
fn looks_secret(token: &str) -> bool {
    let token = trim_token(token);
    if token.starts_with("eyJ") || token.starts_with("sk-") {
        return true;
    }
    token.len() >= 20
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.+/=".contains(c))
}

/// Runs the login in a real podman container from the worker image.
pub struct PodmanLogin {
    spec: LoginSpec,
    /// The podman binary and the network the container joins to reach `ChatGPT`.
    /// The image comes from the Workers setting on each login.
    config: WorkerConfig,
}

impl PodmanLogin {
    /// Builds the spawner from the worker podman config. Only the binary and
    /// the network are used; the login mounts no job directory and passes no
    /// agents or worker volumes.
    #[must_use]
    pub fn new(config: WorkerConfig) -> Self {
        Self::for_spec(LoginSpec::CODEX, config)
    }

    /// As [`PodmanLogin::new`], for the agent `spec` describes.
    #[must_use]
    pub fn for_spec(spec: LoginSpec, config: WorkerConfig) -> Self {
        PodmanLogin { spec, config }
    }
}

impl LoginSpawner for PodmanLogin {
    fn spawn(
        &self,
        name: String,
        image: String,
        home: PathBuf,
    ) -> BoxFuture<'_, std::io::Result<Box<dyn LoginProcess>>> {
        Box::pin(async move {
            // Remove a container left by a login that outlived a crash, so the
            // fixed name is free.
            let _ = remove_container(&self.config, &name).await;
            let mut args: Vec<OsString> = vec![
                "run".into(),
                "--rm".into(),
                "--init".into(),
                "--userns=keep-id".into(),
                "--name".into(),
                name.clone().into(),
                "--env".into(),
                format!("{}={}", self.spec.home_env, self.spec.home_mount).into(),
            ];
            if let Some(network) = &self.config.network {
                args.push("--network".into());
                args.push(network.into());
            }
            let mut mount = OsString::from(home.as_os_str());
            mount.push(format!(":{}", self.spec.home_mount));
            args.push("--volume".into());
            args.push(mount);
            args.push(image.into());
            args.extend(self.spec.command.iter().map(OsString::from));

            let mut child = Command::new(&self.config.podman)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            let (tx, rx) = mpsc::unbounded_channel();
            if let Some(stdout) = child.stdout.take() {
                tokio::spawn(forward_lines(stdout, tx.clone()));
            }
            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(forward_lines(stderr, tx));
            }
            Ok(Box::new(PodmanProcess {
                config: self.config.clone(),
                name,
                child,
                lines: rx,
            }) as Box<dyn LoginProcess>)
        })
    }
}

/// Reads a container stream line by line into the shared channel, so stdout and
/// stderr merge in the order they arrive.
async fn forward_lines(
    stream: impl tokio::io::AsyncRead + Unpin,
    tx: mpsc::UnboundedSender<String>,
) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(line).is_err() {
            break;
        }
    }
}

/// A running podman login container and its merged output.
struct PodmanProcess {
    config: WorkerConfig,
    name: String,
    child: Child,
    lines: mpsc::UnboundedReceiver<String>,
}

impl LoginProcess for PodmanProcess {
    fn next_line(&mut self) -> BoxFuture<'_, Option<String>> {
        Box::pin(self.lines.recv())
    }

    fn wait(&mut self) -> BoxFuture<'_, bool> {
        Box::pin(async move { matches!(self.child.wait().await, Ok(status) if status.success()) })
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Err(err) = remove_container(&self.config, &self.name).await {
                tracing::warn!("removing the login container {}: {err}", self.name);
            }
            let _ = self.child.start_kill();
        })
    }
}
