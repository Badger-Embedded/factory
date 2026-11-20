use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use factory_core::{DEFAULT_EFFORT, Event};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::outcome::{Expect, event_from_result};
use crate::stream::{LogLine, ResultMessage, StreamItem, parse_line};

/// Where the job's checkout appears inside the worker.
pub const WORK_DIR: &str = "/work";
/// Where the job's Claude Code config and sessions appear inside the worker.
pub const CLAUDE_DIR: &str = "/claude";
/// Where the factory plugin appears inside the worker.
pub const PLUGIN_DIR: &str = "/factory-plugin";

const MAX_STDERR: usize = 4096;

/// Settings for every agent run. Claude Code adds a `Co-Authored-By`
/// trailer to commits and a footer to pull request descriptions unless
/// `attribution` empties them, and GitHub copies a branch's co-author
/// trailers into the squash commit on the base branch. Empty strings work
/// on every Claude Code version; `"attribution": false` makes versions
/// before 2.1.281 skip the whole settings value.
pub const AGENT_SETTINGS: &str = r#"{"attribution":{"commit":"","pr":"","sessionUrl":false}}"#;

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
/// factory passes each as an `http` entry to `claude --mcp-config`, pointed at
/// the proxy with the per-run token, so a server credential never enters the
/// worker: the proxy swaps the token for the project's stored headers. The
/// scheduler builds this only when at least one server is active, so a run
/// with none passes no MCP flag.
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

impl McpConfig {
    /// The JSON for `claude --mcp-config`: one `http` entry per active server,
    /// each pointing at the proxy and carrying `Authorization: Bearer <token>`
    /// as its only header. No stored credential appears here; the proxy adds it.
    #[must_use]
    fn to_arg(&self) -> String {
        let servers: serde_json::Map<String, serde_json::Value> = self
            .servers
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    serde_json::json!({
                        "type": "http",
                        "url": format!("{}/mcp/{name}", self.proxy_url),
                        "headers": { "Authorization": format!("Bearer {}", self.token) },
                    }),
                )
            })
            .collect();
        serde_json::json!({ "mcpServers": servers }).to_string()
    }
}

/// Whether a run starts the session or continues it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRun {
    /// Start a session with this UUID, picked by the factory before the run
    /// so it is known even if the factory dies during the run.
    Start(String),
    Resume(String),
}

impl SessionRun {
    /// Resumes session `id` when Claude Code already wrote its transcript
    /// under `claude_dir`, and starts it otherwise. A run that died before
    /// Claude Code wrote anything can then start again with the same ID.
    #[must_use]
    pub fn for_session(claude_dir: &Path, id: &str) -> Self {
        let transcript = format!("{id}.jsonl");
        if contains_file(&claude_dir.join("projects"), &transcript) {
            SessionRun::Resume(id.to_owned())
        } else {
            SessionRun::Start(id.to_owned())
        }
    }
}

fn contains_file(dir: &Path, name: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            contains_file(&path, name)
        } else {
            entry.file_name() == name
        }
    })
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
    /// The job's checkout on the host.
    pub workspace: PathBuf,
    /// The job's Claude Code config directory on the host. It keeps the
    /// session files between runs, so `--resume` finds them.
    pub claude_dir: PathBuf,
    pub session: SessionRun,
    /// The Claude subscription token the factory resolved when this run
    /// started. [`run`] sets it as `CLAUDE_CODE_OAUTH_TOKEN` in the podman
    /// process environment and [`podman_args`] passes `--env
    /// CLAUDE_CODE_OAUTH_TOKEN` with no value, so the token never appears in the
    /// `podman` arguments, the job log or an error message. A token saved on the
    /// settings page reaches the next run this way, with no factory restart.
    pub token: String,
    pub expect: Expect,
    pub prompt: String,
    /// The value for `claude --model`. `None` and an empty string pass no
    /// flag, so Claude Code uses its own default.
    pub model: Option<String>,
    /// The value for `claude --effort`. `None`, such as a request from an
    /// older server, runs at [`DEFAULT_EFFORT`], so a run always passes an
    /// explicit level.
    pub effort: Option<String>,
    /// The MCP servers active for this run, or `None` when none is. `None`
    /// passes no `--mcp-config`, so the arguments are unchanged.
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

/// The arguments after `podman` for one run.
#[must_use]
pub fn podman_args(config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString> {
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
        // Pass the Claude token through from the podman process environment,
        // where `run` sets it, with no value here. The token then stays out of
        // the arguments, the job log and any error message.
        "--env",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "--env",
        &format!("CLAUDE_CONFIG_DIR={CLAUDE_DIR}"),
        "--workdir",
        WORK_DIR,
    ] {
        push(arg);
    }
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
    mount(&run.claude_dir, CLAUDE_DIR, "");
    if let Some(plugin) = &config.plugin_dir {
        mount(plugin, PLUGIN_DIR, ":ro");
    }
    for volume in &config.volumes {
        args.push("--volume".into());
        args.push(volume.into());
    }

    let (session_flag, session_id) = match &run.session {
        SessionRun::Start(id) => ("--session-id", id),
        SessionRun::Resume(id) => ("--resume", id),
    };
    let schema = run.expect.schema().to_string();
    let mut claude = vec![
        config.image.as_str(),
        "claude",
        "-p",
        run.prompt.as_str(),
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "bypassPermissions",
        "--json-schema",
        schema.as_str(),
        "--settings",
        AGENT_SETTINGS,
        session_flag,
        session_id.as_str(),
    ];
    if config.plugin_dir.is_some() {
        claude.extend(["--plugin-dir", PLUGIN_DIR]);
    }
    args.extend(claude.into_iter().map(OsString::from));
    if let Some(model) = run.model.as_deref().filter(|model| !model.is_empty()) {
        args.extend(["--model".into(), model.into()]);
    }
    let effort = run.effort.as_deref().unwrap_or(DEFAULT_EFFORT);
    args.extend(["--effort".into(), effort.into()]);
    // One `http` entry per active MCP server, pointed at the proxy with the run
    // token. A run with no active server adds nothing, so its arguments match a
    // run from before MCP.
    if let Some(mcp) = run.mcp.as_ref().filter(|mcp| !mcp.servers.is_empty()) {
        args.push("--mcp-config".into());
        args.push(mcp.to_arg().into());
    }
    args
}

/// Runs the worker to the end, passing each log line to `log`, and returns
/// the event for the job. Every failure, including podman failing to start,
/// becomes [`Event::AgentFailed`].
pub async fn run(
    config: &WorkerConfig,
    run: &WorkerRun,
    mut log: impl FnMut(LogLine),
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
    let mut child = match Command::new(&config.podman)
        .args(podman_args(config, run))
        // The token reaches the container through podman's own environment and
        // the `--env CLAUDE_CODE_OAUTH_TOKEN` pass-through, not as an argument,
        // so it is absent from the process list.
        .env("CLAUDE_CODE_OAUTH_TOKEN", &run.token)
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

    let mut result: Option<ResultMessage> = None;
    let mut mcp_unavailable: Vec<McpUnavailable> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            for item in parse_line(&line) {
                match item {
                    StreamItem::Log(line) => log(line),
                    StreamItem::Result(message) => result = Some(message),
                    StreamItem::Init(servers) => {
                        for server in servers {
                            if server.status != "connected" {
                                mcp_unavailable.push(McpUnavailable {
                                    reason: format!(
                                        "the agent reported it as {}",
                                        status_label(&server.status)
                                    ),
                                    server: server.name,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    let status = child.wait().await;
    let stderr = stderr_task.await.unwrap_or_default();
    if let Some(result) = result {
        return RunReport {
            event: event_from_result(run.expect, &result),
            session_id: result.session_id.clone(),
            cost_usd: result.total_cost_usd,
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

/// A readable label for an MCP server's reported status. An empty status, which
/// a server can leave out of the `init` line, reads as "not connected".
fn status_label(status: &str) -> &str {
    if status.is_empty() {
        "not connected"
    } else {
        status
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
