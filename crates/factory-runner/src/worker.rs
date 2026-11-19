use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use factory_core::Event;
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

/// Settings shared by every worker.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// The `podman` binary. With `CONTAINER_HOST` set, it talks to that
    /// podman service.
    pub podman: PathBuf,
    pub image: String,
    pub memory: String,
    pub cpus: String,
    /// The podman secret that holds the token from `claude setup-token`.
    pub token_secret: String,
    /// The factory plugin on the host, mounted read-only.
    pub plugin_dir: Option<PathBuf>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            podman: PathBuf::from("podman"),
            image: "localhost/factory-worker:latest".into(),
            memory: "3g".into(),
            cpus: "4".into(),
            token_secret: "claude-token".into(),
            plugin_dir: None,
        }
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
    pub expect: Expect,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub event: Event,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
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
        "--secret",
        &format!(
            "{},type=env,target=CLAUDE_CODE_OAUTH_TOKEN",
            config.token_secret
        ),
        "--env",
        &format!("CLAUDE_CONFIG_DIR={CLAUDE_DIR}"),
        "--workdir",
        WORK_DIR,
    ] {
        push(arg);
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
        session_flag,
        session_id.as_str(),
    ];
    if config.plugin_dir.is_some() {
        claude.extend(["--plugin-dir", PLUGIN_DIR]);
    }
    args.extend(claude.into_iter().map(OsString::from));
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
    let failed = |reason: String| RunReport {
        event: Event::AgentFailed { reason },
        session_id: None,
        cost_usd: None,
    };
    let mut child = match Command::new(&config.podman)
        .args(podman_args(config, run))
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
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            for item in parse_line(&line) {
                match item {
                    StreamItem::Log(line) => log(line),
                    StreamItem::Result(message) => result = Some(message),
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
        };
    }
    let status = status.map_or_else(|err| err.to_string(), |status| status.to_string());
    failed(format!(
        "the worker exited ({status}) without a result{}",
        tail(&stderr)
    ))
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
