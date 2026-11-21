use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

use factory_core::{Agent, Event, PlanUsage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::agent::{AgentItem, RunResult, backend};
use crate::outcome::Expect;
use crate::stream::LogLine;

/// Where the job's checkout appears inside the worker.
pub const WORK_DIR: &str = "/work";
/// Where the factory plugin appears inside the worker.
pub const PLUGIN_DIR: &str = "/factory-plugin";

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
    /// The factory plugin on the host, mounted read-only.
    pub plugin_dir: Option<PathBuf>,
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
            plugin_dir: None,
            volumes: Vec::new(),
            network: None,
        }
    }
}

/// The MCP servers active for one run, and how the worker reaches them. The
/// agent passes each as an `http` entry in its MCP config, pointed at the proxy
/// with the per-run token, so a server credential never enters the worker: the
/// proxy swaps the token for the project's stored headers. The scheduler builds
/// this only when at least one server is active, so a run with none passes no
/// MCP config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    /// The proxy's base URL the worker reaches, such as `http://factory:8081`.
    pub proxy_url: String,
    /// The per-run bearer token the proxy checks. It is the entry's only
    /// header, scoped to this job and these servers, and revoked when the run
    /// ends.
    pub token: String,
    /// The active server names. Each becomes one `http` entry keyed by the
    /// name, pointing at `<proxy_url>/mcp/<name>`. Never empty when this is set.
    pub servers: Vec<String>,
}

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
    let backend = backend(run.agent).expect("the run's agent has a worker backend");
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
    // Pass the credential through from the podman process environment, where
    // `run` sets it, named here with no value. The secret then stays out of the
    // arguments, the job log and any error message. The config directory env
    // points the agent at its mounted config directory.
    args.push("--env".into());
    args.push(backend.credential_env().into());
    args.push("--env".into());
    args.push(format!("{}={}", backend.config_dir_env(), backend.config_dir()).into());
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
    if let Some(plugin) = &config.plugin_dir {
        mount(plugin, PLUGIN_DIR, ":ro");
    }
    for volume in &config.volumes {
        args.push("--volume".into());
        args.push(volume.into());
    }

    args.push(config.image.clone().into());
    args.extend(backend.command(config, run));
    args
}

/// Runs the worker to the end, passing each log line to `log` and each plan
/// usage report to `usage`, and returns the event for the job. Every failure, including podman failing to start,
/// becomes [`Event::AgentFailed`].
pub async fn run(
    config: &WorkerConfig,
    run: &WorkerRun,
    mut log: impl FnMut(LogLine),
    mut usage: impl FnMut(PlanUsage),
) -> RunReport {
    // A review run fails closed. A crash or a missing result is a
    // `ReviewFailed`, so the factory does not merge. Every other run reports
    // `AgentFailed`.
    let failed = |reason: String| RunReport {
        event: if run.expect == Expect::Review {
            Event::ReviewFailed { reason }
        } else {
            Event::AgentFailed { reason }
        },
        session_id: None,
        cost_usd: None,
        mcp_unavailable: Vec::new(),
    };
    let Some(backend) = backend(run.agent) else {
        return failed(format!(
            "the {} agent has no worker backend",
            run.agent.display_name()
        ));
    };
    let mut child = match Command::new(&config.podman)
        .args(podman_args(config, run))
        // The credential reaches the container through podman's own environment
        // and the `--env <name>` pass-through, not as an argument, so it is
        // absent from the process list.
        .env(backend.credential_env(), &run.token)
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

    let mut result: Option<RunResult> = None;
    let mut mcp_unavailable: Vec<McpUnavailable> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            for item in backend.parse(run.expect, &line) {
                match item {
                    AgentItem::Log(line) => log(line),
                    AgentItem::PlanUsage(report) => usage(report),
                    AgentItem::Result(run_result) => result = Some(run_result),
                    AgentItem::Unavailable(unavailable) => mcp_unavailable.push(unavailable),
                }
            }
        }
    }

    let status = child.wait().await;
    let stderr = stderr_task.await.unwrap_or_default();
    if let Some(result) = result {
        return RunReport {
            event: result.event,
            session_id: result.session_id,
            cost_usd: result.cost_usd,
            mcp_unavailable,
        };
    }
    let status = status.map_or_else(|err| err.to_string(), |status| status.to_string());
    RunReport {
        mcp_unavailable,
        ..failed(format!(
            "the worker exited ({status}) without a result{}",
            tail(&stderr)
        ))
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
