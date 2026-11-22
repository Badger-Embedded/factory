//! The GitHub Copilot agent: its implementation of [`AgentBackend`].
//!
//! Everything specific to running a stage with the Copilot CLI lives here: the
//! `copilot -p` command line, the `COPILOT_GITHUB_TOKEN` credential, the
//! `COPILOT_HOME` config directory, the session-resume check, and the JSON
//! Lines parser. See the design doc, "Running Copilot in a worker".
//!
//! The event shapes come from `copilot -p --output-format json` in
//! github/copilot at `1.0.114`, the version the worker image pins. The CLI
//! writes the session-event stream its SDK defines: `session.start` carries
//! `data.sessionId`, each non-ephemeral `assistant.message` the reply in
//! `data.content`, `tool.execution_start` a tool call (`data.toolName`,
//! `data.arguments`, and the `data.mcpServerName`/`data.mcpToolName` of an MCP
//! tool), `session.warning` a `data.message`, and a final top-level `result`
//! record an `exitCode`. The human cause of a failed run is on stderr, not in
//! the stream, so the parser keys a failure off a non-zero `result.exitCode`.

use std::ffi::OsString;
use std::path::Path;

use factory_core::Agent;
use serde_json::Value;

use crate::agent::{AgentBackend, AgentItem, Credential, PromptStyle, RunResult};
use crate::outcome::{Expect, event_from_output, failed_event};
use crate::stream::LogLine;
use crate::worker::{AGENTS_DIR, McpConfig, SessionRun, WorkerConfig, WorkerRun};

/// The Copilot config directory inside the worker, mounted from the job's
/// `copilot` directory on the host and passed to Copilot as `COPILOT_HOME`. It
/// keeps `session-state/<id>/` for a resumed run, next to the login the device
/// flow stores.
const CONFIG_DIR: &str = "/copilot";

/// The environment variable Copilot reads its GitHub token from. It is first in
/// the CLI's lookup order, so the factory's stored login wins over any token in
/// the image. [`run`](crate::worker::run) sets it in podman's process
/// environment, so the token stays out of the arguments and the log.
const CREDENTIAL_ENV: &str = "COPILOT_GITHUB_TOKEN";

/// The environment variable the MCP config references for the run token, as
/// `${FACTORY_MCP_TOKEN}` in each server's `Authorization` header. The CLI
/// expands it from the process environment, where [`run`](crate::worker::run)
/// sets it, so the token stays out of the arguments and the log.
const MCP_TOKEN_ENV: &str = "FACTORY_MCP_TOKEN";

/// The instruction appended to a Copilot prompt. The CLI has no output-schema
/// flag, so the run must end with the JSON itself; [`prompt`](crate::prompt)
/// writes this sentence and then the stage's schema.
const SCHEMA_IN_PROMPT: &str = "This agent has no structured-output mode, so produce the \
     outcome yourself. End the run with one final message that holds only the JSON for your \
     outcome and nothing else: no sentence before it and none after. A single ```json fenced \
     block around the whole message is allowed. The JSON must match this schema:";

/// The review-method paragraph for a Copilot run. Copilot loads the factory's
/// review skill through `--plugin-dir`, as Claude does, so the wording names
/// the skill rather than a path.
const REVIEW_METHOD: &str = "The factory's review skill describes the method. The repository you \
     are reviewing may carry its own review, lint or project skills; use them, since they know \
     rules this prompt does not.\n\n";

/// GitHub Copilot, run with the `copilot -p` CLI in the worker.
pub struct CopilotAgent;

impl AgentBackend for CopilotAgent {
    fn credential(&self) -> Credential {
        // The stored login is a non-expiring `gho_` OAuth token the CLI does not
        // refresh, so it passes by name, unlike Codex's `auth.json`. See the
        // "Copilot CLI facts" gate result in the design doc.
        Credential::Env(CREDENTIAL_ENV)
    }

    fn prompt_style(&self) -> PromptStyle {
        PromptStyle {
            rules_doc: Agent::Copilot.rules_doc(),
            review_method: REVIEW_METHOD,
            schema_in_prompt: Some(SCHEMA_IN_PROMPT),
        }
    }

    fn config_dir_env(&self) -> &'static str {
        "COPILOT_HOME"
    }

    fn config_dir(&self) -> &'static str {
        CONFIG_DIR
    }

    fn host_config_dir(&self) -> &'static str {
        "copilot"
    }

    fn session_for(&self, host_config_dir: &Path, id: &str) -> SessionRun {
        // Copilot keeps each session's events under `session-state/<id>/`, so a
        // stored id with that directory resumes and anything else starts afresh.
        // The factory picks the id, so a run that died before Copilot wrote the
        // directory begins again under the same id.
        if host_config_dir.join("session-state").join(id).is_dir() {
            SessionRun::Resume(id.to_owned())
        } else {
            SessionRun::Start(id.to_owned())
        }
    }

    fn secret_env<'a>(&self, run: &'a WorkerRun) -> Vec<(&'static str, &'a str)> {
        // The MCP run token, by name only, for a run with at least one active
        // server. A run with none passes none, so it gets no token.
        match run.mcp.as_ref().filter(|mcp| !mcp.servers.is_empty()) {
            Some(mcp) => vec![(MCP_TOKEN_ENV, mcp.token.as_str())],
            None => Vec::new(),
        }
    }

    fn command(&self, config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString> {
        let mut copilot: Vec<String> = vec![
            "copilot".into(),
            "-p".into(),
            run.prompt.clone(),
            "--output-format".into(),
            "json".into(),
            // The container is the sandbox, as it is for Claude and Codex, so
            // every tool, path and URL is allowed and nothing waits on a prompt.
            "--allow-all".into(),
            "--no-ask-user".into(),
            "--no-color".into(),
            // Stay on the baked-in release even when a newer one sits in the
            // package cache, and send no telemetry.
            "--no-auto-update".into(),
            "--disable-client-telemetry".into(),
            // The built-in GitHub MCP server would reach GitHub with the login
            // token; a worker gets no GitHub tools, so turn it off.
            "--disable-builtin-mcps".into(),
        ];
        // A new session names the factory's UUID with `--session-id`, as Claude
        // does; a resumed run names it with `--resume`.
        let (session_flag, session_id) = match &run.session {
            SessionRun::Start(id) => ("--session-id", id),
            SessionRun::Resume(id) => ("--resume", id),
        };
        copilot.push(session_flag.into());
        copilot.push(session_id.clone());
        if let Some(model) = run.model.as_deref().filter(|model| !model.is_empty()) {
            copilot.push("--model".into());
            copilot.push(model.into());
        }
        copilot.push("--effort".into());
        copilot.push(
            run.effort
                .as_deref()
                .unwrap_or_else(|| Agent::Copilot.default_effort())
                .into(),
        );
        // One `http` MCP entry per active server, pointed at the proxy. The run
        // token is named, not spelled out: the header carries `${FACTORY_MCP_TOKEN}`
        // and the CLI expands it from the environment. A run with no active
        // server adds nothing.
        if let Some(mcp) = run.mcp.as_ref().filter(|mcp| !mcp.servers.is_empty()) {
            copilot.push("--additional-mcp-config".into());
            copilot.push(mcp_config(mcp));
        }
        // The reviewer run loads the factory's review skill from the mounted
        // agents directory, the same flag Claude uses.
        if config.agents_dir.is_some() {
            copilot.push("--plugin-dir".into());
            copilot.push(AGENTS_DIR.into());
        }
        copilot.into_iter().map(OsString::from).collect()
    }

    fn parse(&self, expect: Expect, line: &str) -> Vec<AgentItem> {
        parse_copilot(expect, line)
    }
}

/// The JSON for `copilot --additional-mcp-config`: one `http` entry per active
/// server, each pointing at the proxy with `Authorization: Bearer ${FACTORY_MCP_TOKEN}`
/// and `"tools": ["*"]`. No stored credential appears here; the CLI expands the
/// variable and the proxy swaps the run token for the project's headers.
fn mcp_config(mcp: &McpConfig) -> String {
    let servers: serde_json::Map<String, Value> = mcp
        .servers
        .iter()
        .map(|name| {
            (
                name.clone(),
                serde_json::json!({
                    "type": "http",
                    "url": format!("{}/mcp/{name}", mcp.proxy_url),
                    "headers": { "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}") },
                    "tools": ["*"],
                }),
            )
        })
        .collect();
    serde_json::json!({ "mcpServers": servers }).to_string()
}

/// Parses one line of `copilot -p --output-format json`. An ephemeral event,
/// such as a streaming `assistant.message_delta`, and a line the factory does
/// not keep give nothing.
fn parse_copilot(expect: Expect, line: &str) -> Vec<AgentItem> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return vec![AgentItem::Log(LogLine::Other(line.to_owned()))];
    };
    // A transient event, such as a streaming delta, is not part of the record
    // the parser reads.
    if value.get("ephemeral").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    let data = value.get("data");
    match value.get("type").and_then(Value::as_str) {
        Some("session.start") => string_at(data, "sessionId")
            .map(|id| AgentItem::Session(id.to_owned()))
            .into_iter()
            .collect(),
        Some("assistant.message") => parse_message(expect, string_at(data, "content")),
        Some("tool.execution_start") => parse_tool_start(data),
        Some("tool.execution_complete") => parse_tool_complete(data),
        Some("session.warning") => string_at(data, "message")
            .filter(|message| !message.trim().is_empty())
            .map(|message| AgentItem::Log(LogLine::Other(message.to_owned())))
            .into_iter()
            .collect(),
        Some("result") => parse_result(expect, &value),
        _ => Vec::new(),
    }
}

/// Turns a non-ephemeral `assistant.message` into its log line and, when it
/// carries text, the result. A message with no content, such as a turn that
/// only requested tools, gives nothing, so the last message with text decides
/// the run.
fn parse_message(expect: Expect, content: Option<&str>) -> Vec<AgentItem> {
    let Some(text) = content.filter(|text| !text.trim().is_empty()) else {
        return Vec::new();
    };
    vec![
        AgentItem::Log(LogLine::Text(text.to_owned())),
        // The last message with text wins: `run` keeps the latest result.
        AgentItem::Result(message_result(expect, text)),
    ]
}

/// Turns the final assistant message into a run result. The text must be the
/// JSON the prompt's schema asked for, with an optional fenced code block around
/// it; text that is not valid JSON fails the run closed for a review and as an
/// agent failure otherwise. Copilot records no cost (option a) and reports the
/// session id on `session.start`, so the run fills both in elsewhere.
fn message_result(expect: Expect, text: &str) -> RunResult {
    let event = match serde_json::from_str::<Value>(unfence(text)) {
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

/// The JSON inside a single fenced code block, or the trimmed text when there is
/// no fence. The prompt allows one fence around the whole message, so a leading
/// triple-backtick with an optional language on its line and a trailing
/// triple-backtick are stripped; anything else is left for the JSON parser to
/// reject.
fn unfence(text: &str) -> &str {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some(inner) = rest.strip_suffix("```") else {
        return text;
    };
    // Drop the language tag or blank that follows the opening fence on its line.
    inner.split_once('\n').map_or("", |(_, body)| body).trim()
}

/// Turns a `tool.execution_start` into one log line: an MCP call as
/// `mcp__<server>__<tool>`, a shell command as an `exec` line, a built-in file
/// edit as an `edit` line, and any other tool as nothing, the way Codex drops a
/// read or a reasoning step.
fn parse_tool_start(data: Option<&Value>) -> Vec<AgentItem> {
    let Some(data) = data else {
        return Vec::new();
    };
    let tool = string_at(Some(data), "toolName").unwrap_or_default();
    // An MCP tool names its server; the proxy routes it, so the log reads like
    // Claude's and Codex's `mcp__<server>__<tool>`.
    if let Some(server) = string_at(Some(data), "mcpServerName")
        .or_else(|| string_at(Some(data), "mcpConfigServerName"))
    {
        let name = string_at(Some(data), "mcpToolName").unwrap_or(tool);
        return vec![AgentItem::Log(LogLine::Tool {
            name: format!("mcp__{server}__{name}"),
            summary: String::new(),
        })];
    }
    // A shell tool carries `shellToolInfo`, absent on every other tool, with the
    // command the CLI runs.
    if let Some(shell) = data.get("shellToolInfo") {
        let command = string_at(Some(shell), "displayCommand")
            .or_else(|| string_at(data.get("arguments"), "command"))
            .unwrap_or(tool);
        return vec![AgentItem::Log(LogLine::Tool {
            name: "exec".into(),
            summary: first_line(command),
        })];
    }
    // A built-in file edit names the path in its arguments.
    if is_edit_tool(tool) {
        let path = edit_path(data.get("arguments")).unwrap_or(tool);
        return vec![AgentItem::Log(LogLine::Tool {
            name: "edit".into(),
            summary: first_line(path),
        })];
    }
    Vec::new()
}

/// A failed `tool.execution_complete` as a log line. A tool that completes does
/// not fail the run, the way a non-fatal Codex item error does not; the log just
/// notes the failure. A successful completion gives nothing, since
/// `tool.execution_start` already logged the call.
fn parse_tool_complete(data: Option<&Value>) -> Vec<AgentItem> {
    let Some(data) = data else {
        return Vec::new();
    };
    if data.get("success").and_then(Value::as_bool) == Some(false) {
        let message = string_at(data.get("error"), "message")
            .or_else(|| string_at(data.get("error"), "code"))
            .unwrap_or("a tool failed");
        return vec![AgentItem::Log(LogLine::Other(format!(
            "tool failed: {message}"
        )))];
    }
    Vec::new()
}

/// The failure for a run whose final `result` reports a non-zero `exitCode`. The
/// human cause is on stderr, not in the stream, so the reason names the code.
/// The run uses this only when no assistant message arrived, so a final message
/// still decides the run.
fn parse_result(expect: Expect, value: &Value) -> Vec<AgentItem> {
    let code = value.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
    if code == 0 {
        return Vec::new();
    }
    vec![AgentItem::Failure(failed_event(
        expect,
        format!("the Copilot run failed: the CLI exited with code {code}"),
    ))]
}

/// Whether a built-in tool name writes a file, so its call logs as an edit.
fn is_edit_tool(tool: &str) -> bool {
    matches!(
        tool,
        "create" | "edit" | "write" | "str_replace" | "multi_edit" | "apply_patch"
    )
}

/// The path a file-edit tool names in its arguments, under whichever key the
/// tool uses.
fn edit_path(arguments: Option<&Value>) -> Option<&str> {
    let arguments = arguments?;
    ["path", "filePath", "file_path", "file"]
        .into_iter()
        .find_map(|key| string_at(Some(arguments), key))
}

/// The string at `key` in `value`, when `value` is an object with a string
/// there.
fn string_at<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value?.get(key).and_then(Value::as_str)
}

/// The first line of some text, trimmed, so a multi-line command reads as one
/// log line, the way Claude's and Codex's tool summaries do.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}
