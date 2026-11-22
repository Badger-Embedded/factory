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

use factory_core::{Agent, Event, PlanUsage};

use crate::claude::ClaudeAgent;
use crate::codex::CodexAgent;
use crate::copilot::CopilotAgent;
use crate::outcome::Expect;
use crate::stream::LogLine;
use crate::worker::{McpUnavailable, SessionRun, WorkerConfig, WorkerRun};

/// How a backend takes its subscription credential. Claude reads a token from
/// an environment variable; Codex reads its `auth.json` from a file in its
/// config directory, which it rewrites when it refreshes the tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// The agent reads the credential from this environment variable.
    /// [`run`](crate::worker::run) sets it in podman's own process environment
    /// from the run's token and [`podman_args`](crate::worker::podman_args)
    /// passes `--env <name>` with no value, so the secret stays out of the
    /// arguments and the log.
    Env(&'static str),
    /// The agent reads the credential from this file in its config directory.
    /// The executor writes it before the run; [`run`](crate::worker::run) reads
    /// it back into the run report afterwards, so the executor can keep a login
    /// the agent refreshed. No `--env` passes it, because the mounted config
    /// directory carries it.
    File(&'static str),
}

/// The per-harness wording a prompt carries, so [`prompt`](crate::prompt) names
/// no harness. The backend returns it, next to the command line and parser that
/// already sit behind [`AgentBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptStyle {
    /// The repository instructions file this harness reads, `CLAUDE.md` for
    /// Claude and `AGENTS.md` for Codex. A prompt that tells the agent to follow
    /// the repository's rules names this file.
    pub rules_doc: &'static str,
    /// The paragraph a review run reads to find its method. Claude loads the
    /// factory's review skill through `--plugin-dir`, so its wording names the
    /// skill; Codex has no such flag, so its wording names the skill's path.
    pub review_method: &'static str,
    /// The instruction a prompt carries when the harness has no output-schema
    /// flag, so the agent must end with the JSON itself. [`prompt`](crate::prompt)
    /// writes this sentence, then the stage's schema, after the outcome
    /// paragraph. Claude and Codex pass the schema to their CLI, so they set
    /// `None` and the prompt carries no schema.
    pub schema_in_prompt: Option<&'static str>,
}

/// What everything agent-specific hides behind, so the worker names no one
/// agent. The Claude implementation is [`ClaudeAgent`].
pub trait AgentBackend: Sync {
    /// How the backend takes its credential: an environment variable for Claude,
    /// a file in the config directory for Codex.
    fn credential(&self) -> Credential;

    /// The per-harness wording a prompt carries: the rules file this harness
    /// reads and the sentence a review run follows to find its method. It keeps
    /// [`prompt`](crate::prompt) from naming a harness.
    fn prompt_style(&self) -> PromptStyle;

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

    /// Writes any files the run needs into its config directory before the
    /// worker starts, such as Codex's `config.toml` and output schema. Claude
    /// needs none, so the default does nothing.
    ///
    /// # Errors
    ///
    /// Fails when a file cannot be written.
    fn prepare(&self, run: &WorkerRun) -> std::io::Result<()> {
        let _ = run;
        Ok(())
    }

    /// Secret values the run passes into the container through podman's process
    /// environment, named by `--env <name>` with no value so they stay out of
    /// the arguments and the log. Codex passes its MCP run token here; Claude
    /// puts its MCP token in the config argument instead and returns none.
    fn secret_env<'a>(&self, run: &'a WorkerRun) -> Vec<(&'static str, &'a str)> {
        let _ = run;
        Vec::new()
    }

    /// The command and its arguments after the image: the whole agent
    /// invocation the worker runs.
    fn command(&self, config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString>;

    /// Log lines the worker writes once, before it reads any output, so a run
    /// that ends with no output still carries them. Copilot notes here that its
    /// runs are not counted in the job cost. The default is none.
    fn start_log(&self) -> Vec<LogLine> {
        Vec::new()
    }

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
    /// The plan usage the agent reported for the account its credential
    /// belongs to.
    PlanUsage(PlanUsage),
    /// The session id the agent chose for itself, such as Codex's thread id,
    /// which the factory resumes with. Claude never emits this, because it runs
    /// under the factory's own UUID.
    Session(String),
    /// The token usage of one completed turn, which the run sums over every
    /// turn. Codex reports this; Claude reports a cost instead.
    Usage(TokenUsage),
    /// The run's final result, resolved from the agent's structured output.
    Result(RunResult),
    /// The agent failed a turn or hit a fatal stream error without a final
    /// result. The run uses this only when no [`Result`](AgentItem::Result)
    /// arrived, so a final message still wins, matching the spec.
    Failure(Event),
}

/// The token counts of a Codex run, summed over its `turn.completed` lines. The
/// executor turns these into an estimated cost from a price table. Claude
/// reports a dollar cost instead and produces none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

impl TokenUsage {
    /// This run's totals plus one more turn's.
    #[must_use]
    pub fn plus(self, turn: TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens + turn.input_tokens,
            cached_input_tokens: self.cached_input_tokens + turn.cached_input_tokens,
            output_tokens: self.output_tokens + turn.output_tokens,
            reasoning_output_tokens: self.reasoning_output_tokens + turn.reasoning_output_tokens,
        }
    }
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

/// The backend for `agent`: [`ClaudeAgent`], [`CodexAgent`] or
/// [`CopilotAgent`]. It returns [`Option`] so an agent registered before its
/// backend lands can report `None`; every agent has one today.
#[must_use]
pub fn backend(agent: Agent) -> Option<&'static dyn AgentBackend> {
    match agent {
        Agent::Claude => Some(&ClaudeAgent),
        Agent::Codex => Some(&CodexAgent),
        Agent::Copilot => Some(&CopilotAgent),
    }
}
