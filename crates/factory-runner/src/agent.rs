//! The agent interface the worker runs behind.
//!
//! The factory runs every stage with an agent. [`worker`](crate::worker) owns
//! the generic `podman run` flags and the run loop; everything agent-specific
//! sits behind [`AgentBackend`]: the command after the image, the credential
//! and config-directory environment variables, the session-resume check, and
//! the parser that turns the agent's stdout into the items the worker consumes.
//! Claude Code is the one agent that runs today, in [`claude`](crate::claude).
//! Codex stands in the registry with no backend yet, so [`backend`] returns
//! `None` for it.

use std::ffi::OsString;
use std::path::Path;

use factory_core::{Agent, Event};

use crate::claude::ClaudeAgent;
use crate::outcome::Expect;
use crate::stream::LogLine;
use crate::worker::{McpUnavailable, SessionRun, WorkerConfig, WorkerRun};

/// What everything agent-specific hides behind, so the worker names no one
/// agent. The Claude implementation is [`ClaudeAgent`].
pub trait AgentBackend: Sync {
    /// The credential environment variable, passed to the container as `--env
    /// NAME` with no value and set by [`run`](crate::worker::run) in the podman
    /// process environment, so the secret never enters the arguments.
    fn credential_env(&self) -> &'static str;

    /// The environment variable that points the agent at its config directory
    /// inside the worker.
    fn config_dir_env(&self) -> &'static str;

    /// The config directory inside the worker, mounted from the run's
    /// [`config_dir`](WorkerRun::config_dir). It keeps the session files a
    /// resumed run reads.
    fn config_dir(&self) -> &'static str;

    /// The config directory's name under the job directory on the host.
    fn host_config_dir(&self) -> &'static str;

    /// Whether session `id` can resume, given the host config directory the run
    /// mounts. A session that left no transcript starts again under the same id,
    /// so a run that died before the agent wrote anything begins afresh.
    fn session_for(&self, host_config_dir: &Path, id: &str) -> SessionRun;

    /// The command and its arguments after the image: the whole agent
    /// invocation the worker runs.
    fn command(&self, config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString>;

    /// Parses one stdout line into the generic items the worker consumes. The
    /// run's [`Expect`] decides what the final result means.
    fn parse(&self, expect: Expect, line: &str) -> Vec<AgentItem>;
}

/// One item the worker reads from an agent run, after the agent's parser turned
/// its own output format into these.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentItem {
    /// A line for the job's live log.
    Log(LogLine),
    /// An active MCP server the agent could not use, named with a reason that
    /// carries no credential.
    Unavailable(McpUnavailable),
    /// The run's final result.
    Result(RunResult),
}

/// An agent run's final result, read from its last stdout line.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    /// The event for the job, resolved from the run's outcome.
    pub event: Event,
    /// The session id to resume next time, when the agent reported one.
    pub session_id: Option<String>,
    /// The run's API cost in USD, when the agent reported it.
    pub cost_usd: Option<f64>,
}

/// The backend for `agent`, or `None` when no runner implements it yet. Only
/// Claude runs today; Codex stands in the registry without a backend.
#[must_use]
pub fn backend(agent: Agent) -> Option<&'static dyn AgentBackend> {
    match agent {
        Agent::Claude => Some(&ClaudeAgent),
        Agent::Codex => None,
    }
}
