//! The Codex agent: its implementation of [`AgentBackend`].
//!
//! Everything specific to running a stage with the Codex CLI lives here: the
//! `codex exec`/`codex exec resume` command line, the `auth.json` file
//! credential, the `CODEX_HOME` config directory, the session-resume check, and
//! the JSON Lines parser. See the design doc, "Running Codex in a worker".
//!
//! The event shapes come from `codex exec --json` in openai/codex at
//! `rust-v0.165.0`, the version the worker image pins: `thread.started` carries
//! a `thread_id`, `turn.completed` a `usage` object, `turn.failed` an
//! `error.message`, a top-level `error` a `message`, and `item.completed` a
//! `ThreadItem` tagged by `type` (`agent_message`, `command_execution`,
//! `file_change`, `mcp_tool_call`, `error` and the rest).

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use std::fmt::Write as _;

use factory_core::{Agent, Cli};
use serde_json::Value;

use crate::agent::{
    AgentBackend, AgentItem, Credential, PromptStyle, RunResult, TokenUsage, write_replacing,
};
use crate::copilot::SCHEMA_IN_PROMPT;
use crate::final_json;
use crate::outcome::{Expect, event_from_output, failed_event};
use crate::stream::LogLine;
use crate::worker::{
    McpServerEntry, ProviderKind, ProviderTarget, STDIO_STARTUP_TIMEOUT_SECS, SessionRun,
    WorkerConfig, WorkerRun,
};

/// The Codex config directory inside the worker, mounted from the job's `codex`
/// directory on the host and passed to Codex as `CODEX_HOME`. It keeps the
/// session rollout files a resumed run reads, next to the `config.toml`, the
/// output schema and the `auth.json` login the executor places.
const CONFIG_DIR: &str = "/codex";

/// The `auth.json` the executor writes and Codex rewrites when it refreshes the
/// subscription tokens. [`run`](crate::worker::run) reads it back into the run
/// report.
const AUTH_FILE: &str = "auth.json";

/// The `config.toml` the backend writes into the config directory. It keeps the
/// login in the file rather than an OS keyring and sets no
/// `project_doc_fallback_filenames`, so Codex reads a repository's `AGENTS.md`
/// and not its `CLAUDE.md`.
const CONFIG_FILE: &str = "config.toml";
const CONFIG_TOML: &str = "cli_auth_credentials_store = \"file\"\n";

/// The provider id a Local run names with `model_provider`, and the variable its
/// key comes from. [`run`](crate::worker::run) sets the key in podman's process
/// environment, so it stays out of the arguments and the log.
const LOCAL_PROVIDER: &str = "local";
const LOCAL_KEY_ENV: &str = "FACTORY_LOCAL_API_KEY";
const OPENROUTER_PROVIDER: &str = "openrouter";
const OPENROUTER_KEY_ENV: &str = "FACTORY_OPENROUTER_API_KEY";

/// Codex's idle timeout on a response stream, 300000 ms unless the provider sets
/// `stream_idle_timeout_ms`. A Local run raises it to at least the model load
/// timeout, because the server sends nothing while it loads the model.
const DEFAULT_STREAM_IDLE_MS: u128 = 300_000;

/// The output schema file the backend writes, passed to `codex exec
/// --output-schema`, which makes the final agent message match it.
const SCHEMA_FILE: &str = "output-schema.json";

/// The environment variable Codex reads the MCP run token from, named in the
/// config as each server's `bearer_token_env_var`. [`run`](crate::worker::run)
/// sets it in podman's process environment, so the token stays out of the
/// arguments and the log.
const MCP_TOKEN_ENV: &str = "FACTORY_MCP_TOKEN";

/// The review-method paragraph for a Codex run. Codex has no `--plugin-dir`
/// flag and finds skills by name, so the wording names the method's path to
/// keep the run off a reviewed repository's own skill called `review`.
const REVIEW_METHOD: &str = "Follow the method in /factory-agents/skills/review/SKILL.md, not any \
     skill named review in the repository you are reviewing. That repository may carry its own \
     lint or project skills; use them, since they know rules this prompt does not.\n\n";

/// Codex, run with the `codex exec` CLI in the worker.
pub struct CodexAgent;

impl AgentBackend for CodexAgent {
    fn credential(&self) -> Credential {
        Credential::File(AUTH_FILE)
    }

    fn prompt_style(&self) -> PromptStyle {
        PromptStyle {
            rules_doc: Cli::Codex.rules_doc(),
            review_method: REVIEW_METHOD,
            schema_in_prompt: None,
        }
    }

    fn provider_prompt_style(&self) -> PromptStyle {
        PromptStyle {
            schema_in_prompt: Some(SCHEMA_IN_PROMPT),
            ..self.prompt_style()
        }
    }

    fn provider_host_config_dir(&self, kind: &ProviderKind) -> &'static str {
        match kind {
            ProviderKind::Local { .. } => "local-codex",
            ProviderKind::OpenRouter => "openrouter-codex",
        }
    }

    fn config_dir_env(&self) -> &'static str {
        "CODEX_HOME"
    }

    fn config_dir(&self) -> &'static str {
        CONFIG_DIR
    }

    fn host_config_dir(&self) -> &'static str {
        "codex"
    }

    fn session_for(&self, host_config_dir: &Path, id: &str) -> SessionRun {
        // Codex picks the thread id itself and names its rollout file after it,
        // so a stored id with a rollout under `sessions` resumes, and anything
        // else starts a fresh session.
        let rollout = format!("{id}.jsonl");
        if contains_file(&host_config_dir.join("sessions"), &rollout) {
            SessionRun::Resume(id.to_owned())
        } else {
            SessionRun::Start(id.to_owned())
        }
    }

    fn prepare(&self, run: &WorkerRun) -> std::io::Result<()> {
        // A Local run has no login, so it writes no credential store line and no
        // output schema: the model gets the schema in its prompt instead.
        let local = run.provider.is_some();
        let mut config = if local {
            String::new()
        } else {
            CONFIG_TOML.to_owned()
        };
        for entry in run.mcp.iter().flat_map(|mcp| &mcp.servers) {
            if let McpServerEntry::Stdio {
                name,
                command,
                args,
            } = entry
            {
                // Codex's startup timeout defaults to 10 seconds, too short for
                // a first download.
                let _ = write!(
                    config,
                    "\n[mcp_servers.{name}]\ncommand = {}\nargs = {}\nstartup_timeout_sec = {STDIO_STARTUP_TIMEOUT_SECS}\n",
                    toml_string(command),
                    toml_array(args)
                );
            }
        }
        write_replacing(&run.config_dir, CONFIG_FILE, &config)?;
        if local {
            return Ok(());
        }
        fs::write(
            run.config_dir.join(SCHEMA_FILE),
            run.expect.strict_schema().to_string(),
        )
    }

    fn secret_env<'a>(&self, run: &'a WorkerRun) -> Vec<(&'static str, &'a str)> {
        // The MCP run token, by name only, for a run with at least one remote
        // server. A run with none, such as one with only stdio servers, passes
        // none, so it gets no token.
        let mut env = Vec::new();
        if let Some(proxy) = run
            .mcp
            .as_ref()
            .filter(|mcp| !mcp.servers.is_empty())
            .and_then(|mcp| mcp.proxy.as_ref())
        {
            env.push((MCP_TOKEN_ENV, proxy.token.as_str()));
        }
        if let Some(provider) = run.provider.as_ref()
            && let Some(key) = provider.key.as_deref()
        {
            env.push((key_env(provider.kind), key));
        }
        env
    }

    fn command(&self, _config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString> {
        let schema = format!("{CONFIG_DIR}/{SCHEMA_FILE}");
        let mut codex: Vec<String> = vec!["codex".into(), "exec".into()];
        // A resumed run names the thread Codex reported before; a new one lets
        // Codex pick its own thread id.
        if let SessionRun::Resume(id) = &run.session {
            codex.push("resume".into());
            codex.push(id.clone());
        }
        codex.push("--json".into());
        // The container is the sandbox, as Claude relies on it for its own
        // bypassed permissions, so Codex runs without its own approvals or
        // sandbox.
        codex.push("--dangerously-bypass-approvals-and-sandbox".into());
        if run.provider.is_none() {
            codex.push("--output-schema".into());
            codex.push(schema);
        }
        if let Some(model) = run.model.as_deref().filter(|model| !model.is_empty()) {
            codex.push("--model".into());
            codex.push(model.into());
        }
        let default_effort = run
            .provider
            .as_ref()
            .map_or(Agent::Codex, |provider| provider.kind.agent())
            .default_effort();
        codex.push("-c".into());
        codex.push(format!(
            "model_reasoning_effort={}",
            run.effort.as_deref().unwrap_or(default_effort)
        ));
        if let Some(provider) = run.provider.as_ref() {
            let (id, table) = match provider.kind {
                ProviderKind::Local { load_timeout } => {
                    (LOCAL_PROVIDER, local_provider(provider, load_timeout))
                }
                ProviderKind::OpenRouter => (OPENROUTER_PROVIDER, openrouter_provider(provider)),
            };
            codex.push("-c".into());
            codex.push(format!("model_providers.{id}={table}"));
            codex.push("-c".into());
            codex.push(format!("model_provider={id}"));
        }
        // A remote MCP server is pointed at the proxy. The run token is passed
        // by name, not value, so it stays out of the arguments; the proxy swaps
        // it for the project's stored headers. A run with no active server adds
        // nothing.
        if let Some(mcp) = run.mcp.as_ref() {
            for entry in &mcp.servers {
                match entry {
                    McpServerEntry::Remote { name: server } => {
                        let Some(proxy) = mcp.proxy.as_ref() else {
                            continue;
                        };
                        codex.push("-c".into());
                        codex.push(format!(
                            "mcp_servers.{server}.url=\"{}/mcp/{server}\"",
                            proxy.url
                        ));
                        codex.push("-c".into());
                        codex.push(format!(
                            "mcp_servers.{server}.bearer_token_env_var=\"{MCP_TOKEN_ENV}\""
                        ));
                    }
                    // Stdio entries live in config.toml, since their arguments
                    // can outgrow one argv element.
                    McpServerEntry::Stdio { .. } => {}
                }
            }
        }
        codex.push(run.prompt.clone());
        codex.into_iter().map(OsString::from).collect()
    }

    fn parse(&self, expect: Expect, line: &str) -> Vec<AgentItem> {
        parse_codex(expect, line, message_result)
    }

    fn parse_provider(&self, expect: Expect, line: &str) -> Vec<AgentItem> {
        parse_codex(expect, line, final_json::message_result)
    }
}

/// The inline TOML table for the Local provider: the server's `/v1` URL over the
/// Responses API, the key's variable when the run has a key, and an idle timeout
/// that outlasts the model load.
fn local_provider(local: &ProviderTarget, load_timeout: Duration) -> String {
    let idle_ms = load_timeout.as_millis().max(DEFAULT_STREAM_IDLE_MS);
    let mut table = format!(
        "{{ name = \"Local\", base_url = {}, wire_api = \"responses\", stream_idle_timeout_ms = {idle_ms}",
        toml_string(&format!("{}/v1", local.base_url)),
    );
    if local.key.is_some() {
        let _ = write!(table, ", env_key = \"{LOCAL_KEY_ENV}\"");
    }
    table.push_str(" }");
    table
}

/// The inline TOML table for the `OpenRouter` provider: `OpenRouter`'s `/v1` URL
/// over the Responses API and the key's variable. It sets no idle timeout,
/// because `OpenRouter` loads no model.
fn openrouter_provider(target: &ProviderTarget) -> String {
    format!(
        "{{ name = \"OpenRouter\", base_url = {}, wire_api = \"responses\", env_key = \"{OPENROUTER_KEY_ENV}\" }}",
        toml_string(&format!("{}/v1", target.base_url)),
    )
}

fn key_env(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Local { .. } => LOCAL_KEY_ENV,
        ProviderKind::OpenRouter => OPENROUTER_KEY_ENV,
    }
}

/// `value` as a TOML basic string, which `-c` parses as the override's value.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn toml_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|value| toml_string(value)).collect();
    format!("[{}]", items.join(","))
}

/// Parses one line of `codex exec --json`. A line the factory does not keep,
/// such as `turn.started`, `item.started` or `item.updated`, gives nothing.
fn parse_codex(
    expect: Expect,
    line: &str,
    final_result: fn(Expect, &str) -> RunResult,
) -> Vec<AgentItem> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return vec![AgentItem::Log(LogLine::Other(line.to_owned()))];
    };
    match value.get("type").and_then(Value::as_str) {
        Some("thread.started") => value
            .get("thread_id")
            .and_then(Value::as_str)
            .map(|id| AgentItem::Session(id.to_owned()))
            .into_iter()
            .collect(),
        Some("turn.completed") => value
            .get("usage")
            .map(|usage| AgentItem::Usage(parse_usage(usage)))
            .into_iter()
            .collect(),
        Some("turn.failed") => failure(
            expect,
            value.pointer("/error/message").and_then(Value::as_str),
        ),
        Some("error") => failure(expect, value.get("message").and_then(Value::as_str)),
        Some("item.completed") => parse_item(expect, value.get("item"), final_result),
        _ => Vec::new(),
    }
}

/// A turn failure or fatal stream error: Codex's message as a log line, and a
/// [`AgentItem::Failure`] with it in the reason. The run uses the failure only
/// when no final agent message arrived.
fn failure(expect: Expect, message: Option<&str>) -> Vec<AgentItem> {
    let message = message.unwrap_or("the Codex run failed without a message");
    vec![
        AgentItem::Log(LogLine::Other(message.to_owned())),
        AgentItem::Failure(failed_event(
            expect,
            format!("the Codex run failed: {message}"),
        )),
    ]
}

/// Turns one `item.completed` item into the log lines and, for the final agent
/// message, the result. A reasoning, web search or to-do item gives nothing, as
/// Claude's thinking blocks do.
fn parse_item(
    expect: Expect,
    item: Option<&Value>,
    final_result: fn(Expect, &str) -> RunResult,
) -> Vec<AgentItem> {
    let Some(item) = item else {
        return Vec::new();
    };
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
            let mut items = Vec::new();
            if !text.trim().is_empty() {
                items.push(AgentItem::Log(LogLine::Text(text.to_owned())));
            }
            // The last agent message wins: `run` keeps the latest result.
            items.push(AgentItem::Result(final_result(expect, text)));
            items
        }
        Some("command_execution") => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut summary = first_line(command);
            if let Some(code) = item.get("exit_code").and_then(Value::as_i64)
                && code != 0
            {
                summary = format!("{summary} (exit {code})");
            }
            vec![AgentItem::Log(LogLine::Tool {
                name: "exec".into(),
                summary,
            })]
        }
        Some("file_change") => {
            let paths: Vec<&str> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(|change| change.get("path").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            vec![AgentItem::Log(LogLine::Tool {
                name: "edit".into(),
                summary: first_line(&paths.join(", ")),
            })]
        }
        Some("mcp_tool_call") => {
            let server = item
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or_default();
            vec![AgentItem::Log(LogLine::Tool {
                name: format!("mcp__{server}__{tool}"),
                summary: String::new(),
            })]
        }
        Some("error") => {
            // A non-fatal item error, such as a startup warning, is a log line
            // and does not fail the run.
            let message = item
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            vec![AgentItem::Log(LogLine::Other(message.to_owned()))]
        }
        _ => Vec::new(),
    }
}

/// Resolves a final agent message into a run result. The text must be the JSON
/// the output schema asked for; text that is not valid JSON fails the run
/// closed for a review and as an agent failure otherwise. Codex reports no
/// session id on this line, so the run fills it in from `thread.started`, and no
/// cost, so the executor estimates it from the summed usage.
fn message_result(expect: Expect, text: &str) -> RunResult {
    let event = match serde_json::from_str::<Value>(text.trim()) {
        Ok(value) => event_from_output(expect, Some(value)),
        Err(err) => failed_event(
            expect,
            format!("the agent's final message is not valid JSON: {err}"),
        ),
    };
    RunResult {
        event,
        session_id: None,
        cost_usd: None,
    }
}

/// The token counts of one `turn.completed` line. A negative or absent count
/// reads as zero.
fn parse_usage(usage: &Value) -> TokenUsage {
    let count =
        |key: &str| u64::try_from(usage.get(key).and_then(Value::as_i64).unwrap_or(0)).unwrap_or(0);
    TokenUsage {
        input_tokens: count("input_tokens"),
        cached_input_tokens: count("cached_input_tokens"),
        output_tokens: count("output_tokens"),
        reasoning_output_tokens: count("reasoning_output_tokens"),
    }
}

/// The first line of some text, trimmed, so a multi-line command reads as one
/// log line, the way Claude's tool summaries do.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
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
