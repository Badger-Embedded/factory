use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use factory_core::{Agent, Cli, Event, PlanUsage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::agent::{AgentItem, Credential, RunResult, TokenUsage, backend_for};
use crate::outcome::{Expect, failed_event};
use crate::packages;
use crate::stream::LogLine;

/// Where the job's checkout appears inside the worker.
pub const WORK_DIR: &str = "/work";
/// Where the factory's bundled agent material appears inside the worker.
pub const AGENTS_DIR: &str = "/factory-agents";

const MAX_STDERR: usize = 4096;

/// Settings shared by every worker.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// The `podman` binary. With `CONTAINER_HOST` set, it talks to that
    /// podman service.
    pub podman: PathBuf,
    pub image: String,
    pub memory: String,
    pub cpus: String,
    /// The factory's bundled agent material on the host, mounted read-only.
    pub agents_dir: Option<PathBuf>,
    /// More `--volume` values for every worker, such as a named volume for
    /// the nix store that all workers share.
    pub volumes: Vec<String>,
    /// The podman network every worker joins, so it can reach the factory's
    /// MCP proxy by the factory container's name. `None` leaves podman's
    /// default network, which has no route to the proxy. The dashboard port is
    /// published to the host loopback alone, so a worker on this network cannot
    /// reach it.
    pub network: Option<String>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            podman: PathBuf::from("podman"),
            image: "localhost/factory-worker:latest".into(),
            memory: "3g".into(),
            cpus: "4".into(),
            agents_dir: None,
            volumes: Vec::new(),
            network: None,
        }
    }
}

/// The MCP servers active for one run, and how the worker reaches them. A
/// remote server is an `http` entry in the agent's MCP config, pointed at the
/// proxy with the per-run token, so a server credential never enters the worker:
/// the proxy swaps the token for the project's stored headers. A stdio server is
/// a command the agent starts itself inside the worker, so it passes through no
/// proxy and carries no token. The scheduler builds this only when at least one
/// server is active, so a run with none passes no MCP config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    /// How the worker reaches the proxy. Set only when a remote server is
    /// active, so a run with only stdio servers has no token and no URL.
    pub proxy: Option<McpProxy>,
    /// The active servers. Each becomes one entry in the agent's MCP config.
    /// Never empty when this is set.
    pub servers: Vec<McpServerEntry>,
}

/// The proxy a run's remote servers go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProxy {
    /// The proxy's base URL the worker reaches, such as `http://factory:8081`.
    pub url: String,
    /// The per-run bearer token the proxy checks. It is the entry's only
    /// header, scoped to this job and these servers, and revoked when the run
    /// ends.
    pub token: String,
}

impl McpConfig {
    /// The active server names, in config order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.servers.iter().map(McpServerEntry::name).collect()
    }

    /// Whether any active server is a stdio server.
    #[must_use]
    pub fn has_stdio(&self) -> bool {
        self.servers
            .iter()
            .any(|server| matches!(server, McpServerEntry::Stdio { .. }))
    }
}

/// One active MCP server in a run's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerEntry {
    /// A remote server behind the proxy. It becomes one `http` entry keyed by
    /// the name, pointing at `<proxy url>/mcp/<name>`.
    Remote { name: String },
    /// A command the agent starts as a child process in the worker and talks to
    /// over stdin and stdout. `args` is the registry's base arguments followed
    /// by the project's extra arguments.
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
    },
}

impl McpServerEntry {
    /// The server's name, the key of its entry in the agent's MCP config.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Remote { name } | Self::Stdio { name, .. } => name,
        }
    }
}

/// How long the agent waits for a stdio server to start, in seconds. A first
/// start of a package runner such as `npx` downloads the package.
pub const STDIO_STARTUP_TIMEOUT_SECS: u64 = 120;

/// Whether a run starts the session or continues it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRun {
    /// Start a session with this UUID, picked by the factory before the run
    /// so it is known even if the factory dies during the run.
    Start(String),
    Resume(String),
}

/// Removes container `name` if it exists, such as a worker that outlived
/// a factory crash.
///
/// # Errors
///
/// Fails when podman cannot run or reports an error.
pub async fn remove_container(config: &WorkerConfig, name: &str) -> Result<(), String> {
    let output = Command::new(&config.podman)
        .args(["rm", "--force", "--ignore", name])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|err| format!("could not start {}: {err}", config.podman.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// One agent run.
#[derive(Debug, Clone)]
pub struct WorkerRun {
    /// The container name. Naming each run lets the factory remove a
    /// container that outlived a crash.
    pub name: String,
    /// The agent this run uses. It picks the command after the image, the
    /// credential and config-directory environment variables, the
    /// session-resume check and the parser.
    pub agent: Agent,
    /// The job's checkout on the host.
    pub workspace: PathBuf,
    /// The agent's config directory on the host. It keeps the session files
    /// between runs, so a resumed run finds them.
    pub config_dir: PathBuf,
    pub session: SessionRun,
    /// The subscription credential the factory resolved when this run started.
    /// [`run`] sets it in the podman process environment under the agent's
    /// credential variable and [`podman_args`] passes `--env <name>` with no
    /// value, so the secret never appears in the `podman` arguments, the job log
    /// or an error message. A credential saved on the settings page reaches the
    /// next run this way, with no factory restart.
    pub token: String,
    pub expect: Expect,
    pub prompt: String,
    /// The model for the run. `None` and an empty string pass no model flag,
    /// so the agent uses its own default.
    pub model: Option<String>,
    /// The effort level for the run. `None`, such as a request from an older
    /// server, runs at the agent's default, so a run always passes an explicit
    /// level.
    pub effort: Option<String>,
    /// The MCP servers active for this run, or `None` when none is. `None`
    /// passes no MCP config, so the arguments are unchanged.
    pub mcp: Option<McpConfig>,
    /// The project's nixpkgs packages. A non-empty list runs the agent inside
    /// `nix shell` with them on `PATH`; an empty one changes nothing.
    pub packages: Vec<String>,
    /// The local server this run talks to instead of an agent's subscription.
    /// `None` runs the agent on its own login. With a target the run starts
    /// `local.cli`, carries no subscription credential and reads no credential
    /// file back.
    pub local: Option<LocalTarget>,
}

impl WorkerRun {
    /// The CLI that runs this stage: the Local target's, else the agent's own.
    #[must_use]
    pub fn cli(&self) -> Option<Cli> {
        self.local
            .as_ref()
            .map(|local| local.cli)
            .or_else(|| self.agent.cli())
    }
}

/// The OpenAI-compatible server a Local run uses and the CLI that drives it.
#[derive(Clone, PartialEq, Eq)]
pub struct LocalTarget {
    /// The CLI that runs the model.
    pub cli: Cli,
    /// The server's base URL, with no trailing slash and no `/v1`.
    pub base_url: String,
    /// The API key the run resolved, when there is one. It reaches the worker
    /// only as `--env <name>` with the value in podman's own environment.
    pub key: Option<String>,
    /// How long the server may take to load a model. The run raises the CLI's
    /// request timeout to at least this.
    pub load_timeout: Duration,
}

impl std::fmt::Debug for LocalTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalTarget")
            .field("cli", &self.cli)
            .field("base_url", &self.base_url)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("load_timeout", &self.load_timeout)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub event: Event,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    /// The active MCP servers Claude Code could not connect to, read from the
    /// `mcp_servers` list in the stream-json `system`/`init` line. Each causes a
    /// warning on the job. Empty for a run with no active server, or one whose
    /// servers all connected.
    pub mcp_unavailable: Vec<McpUnavailable>,
    /// The backend's credential file read back after the run, for a
    /// [`Credential::File`] backend such as Codex, whose CLI rewrites its
    /// `auth.json` when it refreshes the tokens. The executor compares it with
    /// the stored login and keeps the newer one. `None` for an environment
    /// credential, or when the file was absent.
    pub credential: Option<String>,
    /// The token usage summed over a Codex run's `turn.completed` lines, which
    /// the executor turns into an estimated cost. `None` for a run that reports
    /// a dollar cost instead, such as Claude's.
    pub token_usage: Option<TokenUsage>,
}

/// An active MCP server that was not `connected` in the run's `init` line: its
/// name and a short reason with no credential in it. The factory records it as
/// a warning on the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpUnavailable {
    pub server: String,
    pub reason: String,
}

/// The arguments after `podman` for one run. The generic `podman run` flags
/// are here; the agent's [`command`](crate::AgentBackend::command) and the
/// agent's credential and config-directory variables come from its backend, so
/// no one agent is named here.
///
/// # Panics
///
/// Panics when the run's agent has no worker backend. The factory builds a run
/// only for an agent that can run, so this is an unreachable invariant.
#[must_use]
pub fn podman_args(config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString> {
    let backend = run
        .cli()
        .and_then(backend_for)
        .expect("the run's CLI has a worker backend");
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |arg: &str| args.push(arg.into());
    for arg in [
        "run",
        "--rm",
        "--init",
        "--userns=keep-id",
        "--name",
        &run.name,
        "--memory",
        &config.memory,
        "--cpus",
        &config.cpus,
    ] {
        push(arg);
    }
    // Pass an environment credential through from the podman process
    // environment, where `run` sets it, named here with no value. The secret
    // then stays out of the arguments, the job log and any error message. A
    // file credential, such as Codex's `auth.json`, is in the mounted config
    // directory instead, so it needs no `--env`. The config directory env
    // points the agent at its mounted config directory.
    if run.local.is_none()
        && let Credential::Env(name) = backend.credential()
    {
        args.push("--env".into());
        args.push(name.into());
    }
    for (name, value) in backend.local_env(run) {
        args.push("--env".into());
        args.push(format!("{name}={value}").into());
    }
    args.push("--env".into());
    args.push(format!("{}={}", backend.config_dir_env(), backend.config_dir()).into());
    // Each other secret the run passes by name, such as Codex's MCP run token,
    // named here with no value and set by `run` in the podman process
    // environment, so it too stays out of the arguments and the log.
    for (name, _) in backend.secret_env(run) {
        args.push("--env".into());
        args.push(name.into());
    }
    args.push("--workdir".into());
    args.push(WORK_DIR.into());
    // The worker joins the factory's podman network so it can reach the MCP
    // proxy by the factory container's name. It is added before the image, with
    // the other `podman run` flags.
    if let Some(network) = &config.network {
        args.push("--network".into());
        args.push(network.into());
    }
    let mut mount = |host: &PathBuf, target: &str, options: &str| {
        let mut volume = OsString::from(host.as_os_str());
        volume.push(format!(":{target}{options}"));
        args.push("--volume".into());
        args.push(volume);
    };
    mount(&run.workspace, WORK_DIR, "");
    mount(&run.config_dir, backend.config_dir(), "");
    if let Some(agents) = &config.agents_dir {
        mount(agents, AGENTS_DIR, ":ro");
    }
    for volume in &config.volumes {
        args.push("--volume".into());
        args.push(volume.into());
    }

    args.push(config.image.clone().into());
    args.extend(packages::wrap(&run.packages, backend.command(config, run)));
    args
}

/// Runs the worker to the end, passing each log line to `log`, each plan usage
/// report to `usage`, and the id an agent reports for its own session, such as
/// Codex's thread id, to `session` as it arrives, and returns the event for the
/// job. Every failure, including podman failing to start, becomes
/// [`Event::AgentFailed`].
pub async fn run(
    config: &WorkerConfig,
    run: &WorkerRun,
    mut log: impl FnMut(LogLine),
    mut usage: impl FnMut(PlanUsage),
    mut session: impl FnMut(&str),
) -> RunReport {
    // A review run fails closed. A crash or a missing result is a
    // `ReviewFailed`, so the factory does not merge. Every other run reports
    // `AgentFailed`.
    let failed = |reason: String| RunReport {
        event: failed_event(run.expect, reason),
        session_id: None,
        cost_usd: None,
        mcp_unavailable: Vec::new(),
        credential: None,
        token_usage: None,
    };
    let Some(backend) = run.cli().and_then(backend_for) else {
        return failed(format!(
            "the {} agent has no worker backend",
            run.agent.display_name()
        ));
    };
    // The backend's own files, such as Codex's config.toml and output schema,
    // go into the config directory before the container mounts it.
    if let Err(err) = backend.prepare(run) {
        return failed(format!(
            "could not write the {} config: {err}",
            run.agent.id()
        ));
    }
    // A Local run has no subscription, so a backend's note about its cost
    // does not apply; the factory writes the Local cost line itself.
    if run.local.is_none() {
        for line in backend.start_log() {
            log(line);
        }
    }
    let mut command = Command::new(&config.podman);
    command.args(podman_args(config, run));
    // An environment credential reaches the container through podman's own
    // environment and the `--env <name>` pass-through, not as an argument, so it
    // is absent from the process list. A file credential is in the mounted
    // config directory, so nothing is set here for it.
    if run.local.is_none()
        && let Credential::Env(name) = backend.credential()
    {
        command.env(name, &run.token);
    }
    // Each other secret, such as Codex's MCP run token, the same way.
    for (name, value) in backend.secret_env(run) {
        command.env(name, value);
    }
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return failed(format!(
                "could not start {}: {err}",
                config.podman.display()
            ));
        }
    };

    let mut stderr = child.stderr.take().map(BufReader::new);
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_string(&mut text).await;
        }
        text
    });

    let redactor = Redactor::new(run.local.as_ref().and_then(|local| local.key.as_deref()));
    let mut collected = Collected::default();
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let items = if run.local.is_some() {
                backend.parse_local(run.expect, &line)
            } else {
                backend.parse(run.expect, &line)
            };
            for item in items {
                let item = redactor.item(item);
                // A Local run says nothing about the subscription's plan usage,
                // whatever the CLI reports.
                if run.local.is_some() && matches!(item, AgentItem::PlanUsage(_)) {
                    continue;
                }
                collected.absorb(item, &mut log, &mut usage, &mut session);
            }
        }
    }

    let status = child.wait().await;
    let stderr = redactor.apply_raw(&stderr_task.await.unwrap_or_default());
    // A file credential, such as Codex's `auth.json`, read back after the run so
    // the executor can keep a login the agent refreshed. The file may be gone,
    // which reads as `None`.
    let credential = match backend.credential() {
        Credential::File(name) if run.local.is_none() => {
            fs::read_to_string(run.config_dir.join(name)).ok()
        }
        _ => None,
    };
    if !run.packages.is_empty()
        && collected.result.is_none()
        && status
            .as_ref()
            .is_ok_and(|status| status.code() == Some(packages::FAILED_CODE))
    {
        return failed(packages::failure(&run.packages, &tail(&stderr)));
    }
    collected.into_report(run.expect, credential, status, &stderr)
}

/// Replaces a Local run's key in the text the run publishes, so a tool or a
/// provider error that echoes it never reaches the job log or a failure reason.
/// Stdout is parsed as the CLI wrote it and redacted after decoding, so a key
/// that equals a protocol marker cannot break the parse.
struct Redactor {
    key: Option<String>,
    /// The key as it reads inside a JSON string, when that differs.
    escaped: Option<String>,
}

impl Redactor {
    fn new(key: Option<&str>) -> Self {
        let key = key.filter(|key| !key.is_empty());
        let escaped = key
            .and_then(|key| serde_json::to_string(key).ok())
            .and_then(|json| {
                json.strip_prefix('"')
                    .and_then(|json| json.strip_suffix('"'))
                    .map(str::to_owned)
            })
            .filter(|escaped| Some(escaped.as_str()) != key);
        Redactor {
            key: key.map(str::to_owned),
            escaped,
        }
    }

    fn apply(&self, text: &str) -> String {
        match &self.key {
            Some(key) => text.replace(key.as_str(), "[redacted]"),
            None => text.to_owned(),
        }
    }

    /// Raw output, where the key may appear JSON-escaped.
    fn apply_raw(&self, text: &str) -> String {
        let text = self.apply(text);
        match &self.escaped {
            Some(escaped) => text.replace(escaped.as_str(), "[redacted]"),
            None => text,
        }
    }

    fn item(&self, item: AgentItem) -> AgentItem {
        if self.key.is_none() {
            return item;
        }
        match item {
            AgentItem::Log(line) => AgentItem::Log(match line {
                LogLine::Text(text) => LogLine::Text(self.apply(&text)),
                LogLine::Tool { name, summary } => LogLine::Tool {
                    name: self.apply(&name),
                    summary: self.apply(&summary),
                },
                LogLine::Other(text) => LogLine::Other(self.apply(&text)),
            }),
            AgentItem::Unavailable(McpUnavailable { server, reason }) => {
                AgentItem::Unavailable(McpUnavailable {
                    server: self.apply(&server),
                    reason: self.apply(&reason),
                })
            }
            AgentItem::Result(mut result) => {
                result.event = self.event(result.event);
                AgentItem::Result(result)
            }
            AgentItem::Failure(event) => AgentItem::Failure(self.event(event)),
            other => other,
        }
    }

    fn event(&self, event: Event) -> Event {
        let Ok(mut value) = serde_json::to_value(&event) else {
            return event;
        };
        self.value(&mut value);
        serde_json::from_value(value).unwrap_or(event)
    }

    fn value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.apply(text),
            serde_json::Value::Array(items) => items.iter_mut().for_each(|v| self.value(v)),
            serde_json::Value::Object(map) => map.values_mut().for_each(|v| self.value(v)),
            _ => {}
        }
    }
}

/// What one run's stdout gives, gathered as the lines arrive.
#[derive(Default)]
struct Collected {
    result: Option<RunResult>,
    mcp_unavailable: Vec<McpUnavailable>,
    /// The agent's own session id, such as Codex's thread id. Empty for Claude,
    /// which reports a session id on its result line.
    agent_session: Option<String>,
    /// The token usage summed over the run's turns. Empty for Claude, which
    /// reports a dollar cost instead.
    token_usage: Option<TokenUsage>,
    /// A turn failure or fatal stream error, used only when no result arrives,
    /// so a final agent message still decides the run.
    failure: Option<Event>,
}

impl Collected {
    /// Takes one parsed item, sending log lines and plan usage straight to the
    /// callbacks.
    fn absorb(
        &mut self,
        item: AgentItem,
        log: &mut impl FnMut(LogLine),
        usage: &mut impl FnMut(PlanUsage),
        session: &mut impl FnMut(&str),
    ) {
        match item {
            AgentItem::Log(line) => log(line),
            AgentItem::PlanUsage(report) => usage(report),
            AgentItem::Result(result) => self.result = Some(result),
            AgentItem::Unavailable(unavailable) => self.mcp_unavailable.push(unavailable),
            AgentItem::Session(id) => {
                // Hand the id to the caller before keeping it, so the scheduler
                // can store it the moment it arrives.
                session(&id);
                self.agent_session = Some(id);
            }
            AgentItem::UsageTotal(total) => self.token_usage = Some(total),
            AgentItem::Usage(turn) => {
                self.token_usage = Some(self.token_usage.unwrap_or_default().plus(turn));
            }
            AgentItem::Failure(event) => self.failure = Some(event),
        }
    }

    /// The run's report: the final result when one arrived, else the agent's own
    /// failure, else a bare exit. The report carries the session id and usage either
    /// way, so a run killed after its session started can resume on Retry.
    fn into_report(
        self,
        expect: Expect,
        credential: Option<String>,
        status: std::io::Result<std::process::ExitStatus>,
        stderr: &str,
    ) -> RunReport {
        if let Some(result) = self.result {
            return RunReport {
                event: result.event,
                session_id: result.session_id.or(self.agent_session),
                cost_usd: result.cost_usd,
                mcp_unavailable: self.mcp_unavailable,
                credential,
                token_usage: self.token_usage,
            };
        }
        let event = self.failure.unwrap_or_else(|| {
            let status = status.map_or_else(|err| err.to_string(), |status| status.to_string());
            failed_event(
                expect,
                format!(
                    "the worker exited ({status}) without a result{}",
                    tail(stderr)
                ),
            )
        });
        RunReport {
            event,
            session_id: self.agent_session,
            cost_usd: None,
            mcp_unavailable: self.mcp_unavailable,
            credential,
            token_usage: self.token_usage,
        }
    }
}

fn tail(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return String::new();
    }
    let start = stderr
        .char_indices()
        .rev()
        .nth(MAX_STDERR)
        .map_or(0, |(index, _)| index);
    format!(": {}", &stderr[start..])
}
