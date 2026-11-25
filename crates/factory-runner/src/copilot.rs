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

use factory_core::{Agent, Cli};
use serde_json::Value;

use crate::agent::{AgentBackend, AgentItem, Credential, PromptStyle, write_replacing};
use crate::final_json::message_result;
use crate::outcome::{Expect, failed_event};
use crate::stream::LogLine;
use crate::worker::{
    AGENTS_DIR, McpConfig, McpServerEntry, McpUnavailable, SessionRun, WorkerConfig, WorkerRun,
};

/// The Copilot config directory inside the worker, mounted from the job's
/// `copilot` directory on the host and passed to Copilot as `COPILOT_HOME`. It
/// keeps `session-state/<id>/` for a resumed run, next to the login the device
/// flow stores.
const CONFIG_DIR: &str = "/copilot";
const MCP_CONFIG_FILE: &str = "factory-mcp.json";

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
/// writes this text and then the stage's schema.
const SCHEMA_IN_PROMPT: &str = "This agent has no structured-output mode, so produce the \
     outcome yourself. End the run with one final message that holds only the JSON for your \
     outcome and nothing else: no sentence before it and none after. The factory parses that \
     message as JSON, and any text outside the JSON fails the run. Put a summary, an \
     explanation or a note to the requester inside the JSON fields the schema offers, such as \
     the pull request body, the spec or the review feedback, or leave it out. Only the final \
     message is restricted; earlier messages may hold prose. A single ```json fenced block \
     around the JSON is allowed. The JSON must match this schema:";

/// The note [`start_log`](AgentBackend::start_log) writes at the start of each
/// run. See Job cost in the design doc.
const COST_NOTE: &str = "Copilot runs are not counted in the job cost.";

/// The review-method paragraph for a Copilot run. Every run passes
/// `--plugin-dir`, so the wording names the skill rather than a path.
const REVIEW_METHOD: &str = "The factory's review skill describes the method. The repository you \
     are reviewing may carry its own review, lint or project skills; use them, since they know \
     rules this prompt does not.\n\n";

/// GitHub Copilot, run with the `copilot -p` CLI in the worker.
pub struct CopilotAgent;

impl AgentBackend for CopilotAgent {
    fn credential(&self) -> Credential {
        Credential::Env(CREDENTIAL_ENV)
    }

    fn prompt_style(&self) -> PromptStyle {
        PromptStyle {
            rules_doc: Cli::Copilot.rules_doc(),
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

    fn prepare(&self, run: &WorkerRun) -> std::io::Result<()> {
        match run.mcp.as_ref().filter(|mcp| mcp.has_stdio()) {
            Some(mcp) => write_replacing(&run.config_dir, MCP_CONFIG_FILE, &mcp_config(mcp)),
            None => Ok(()),
        }
    }

    fn session_for(&self, host_config_dir: &Path, id: &str) -> SessionRun {
        if host_config_dir.join("session-state").join(id).is_dir() {
            SessionRun::Resume(id.to_owned())
        } else {
            SessionRun::Start(id.to_owned())
        }
    }

    fn secret_env<'a>(&self, run: &'a WorkerRun) -> Vec<(&'static str, &'a str)> {
        match run
            .mcp
            .as_ref()
            .filter(|mcp| !mcp.servers.is_empty())
            .and_then(|mcp| mcp.proxy.as_ref())
        {
            Some(proxy) => vec![(MCP_TOKEN_ENV, proxy.token.as_str())],
            None => Vec::new(),
        }
    }

    fn command(&self, _config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString> {
        let (session_flag, session_id) = match &run.session {
            SessionRun::Start(id) => ("--session-id", id),
            SessionRun::Resume(id) => ("--resume", id),
        };
        let mut copilot: Vec<String> = vec![
            "copilot".into(),
            "-p".into(),
            run.prompt.clone(),
            "--output-format".into(),
            "json".into(),
            "--allow-all".into(),
            "--no-ask-user".into(),
            "--no-color".into(),
            // Without this, a newer version in the package cache runs instead of
            // the image's pinned one.
            "--no-auto-update".into(),
            "--disable-client-telemetry".into(),
            // The built-in GitHub MCP server would reach GitHub with the login
            // token, so a worker gets no GitHub tools.
            "--disable-builtin-mcps".into(),
            session_flag.into(),
            session_id.clone(),
        ];
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
        if let Some(mcp) = run.mcp.as_ref().filter(|mcp| !mcp.servers.is_empty()) {
            copilot.push("--additional-mcp-config".into());
            if mcp.has_stdio() {
                copilot.push(format!("@{CONFIG_DIR}/{MCP_CONFIG_FILE}"));
            } else {
                copilot.push(mcp_config(mcp));
            }
        }
        // The image bakes the review skills at `/factory-agents` whether or not
        // the factory mounts its own copy there, so the flag always resolves and
        // the review wording can name the skill rather than a path.
        copilot.push("--plugin-dir".into());
        copilot.push(AGENTS_DIR.into());
        copilot.into_iter().map(OsString::from).collect()
    }

    fn start_log(&self) -> Vec<LogLine> {
        vec![LogLine::Other(COST_NOTE.to_owned())]
    }

    fn parse(&self, expect: Expect, line: &str) -> Vec<AgentItem> {
        parse_copilot(expect, line)
    }
}

/// The JSON for `copilot --additional-mcp-config`: one entry per active server,
/// each with `"tools": ["*"]`. A remote server is an `http` entry pointing at the
/// proxy with `Authorization: Bearer ${FACTORY_MCP_TOKEN}`. No stored credential
/// appears here; the CLI expands the variable and the proxy swaps the run token
/// for the project's headers. A stdio server is a `local` entry with its command
/// and arguments. Copilot has no startup timeout setting, so it keeps its own.
fn mcp_config(mcp: &McpConfig) -> String {
    let servers: serde_json::Map<String, Value> = mcp
        .servers
        .iter()
        .filter_map(|server| match server {
            McpServerEntry::Remote { name } => {
                let proxy = mcp.proxy.as_ref()?;
                Some((
                    name.clone(),
                    serde_json::json!({
                        "type": "http",
                        "url": format!("{}/mcp/{name}", proxy.url),
                        "headers": { "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}") },
                        "tools": ["*"],
                    }),
                ))
            }
            McpServerEntry::Stdio {
                name,
                command,
                args,
            } => Some((
                name.clone(),
                serde_json::json!({
                    "type": "local",
                    "command": command,
                    "args": args,
                    "tools": ["*"],
                }),
            )),
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
    let data = value.get("data");
    // These two are ephemeral, so they come before the ephemeral check.
    match value.get("type").and_then(Value::as_str) {
        Some("session.mcp_server_status_changed") => return mcp_status_changed(data),
        Some("session.mcp_servers_loaded") => return mcp_servers_loaded(data),
        _ => {}
    }
    if value.get("ephemeral").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    match value.get("type").and_then(Value::as_str) {
        Some("session.start") => string_at(data, "sessionId")
            .map(|id| AgentItem::Session(id.to_owned()))
            .into_iter()
            .collect(),
        Some("assistant.message") => parse_message(expect, data),
        Some("tool.execution_start") => parse_tool_start(data),
        Some("tool.execution_complete") => parse_tool_complete(data),
        Some("session.warning") => string_at(data, "message")
            .filter(|message| !message.trim().is_empty())
            .map(|message| AgentItem::Log(LogLine::Other(message.to_owned())))
            .into_iter()
            .collect(),
        Some("session.usage_checkpoint") => checkpoint_log(data).into_iter().collect(),
        Some("result") => parse_result(expect, &value),
        _ => Vec::new(),
    }
}

/// A server the agent reports as failed, such as a stdio command that would not
/// start. The reason names the status only, since the CLI's error text can quote
/// the command's output.
fn mcp_unavailable(server: &str, status: &str) -> AgentItem {
    AgentItem::Unavailable(McpUnavailable {
        server: server.to_owned(),
        reason: format!("the agent reported it as {status}"),
    })
}

fn mcp_status_changed(data: Option<&Value>) -> Vec<AgentItem> {
    match (string_at(data, "serverName"), string_at(data, "status")) {
        (Some(server), Some("failed")) => vec![mcp_unavailable(server, "failed")],
        _ => Vec::new(),
    }
}

fn mcp_servers_loaded(data: Option<&Value>) -> Vec<AgentItem> {
    data.and_then(|data| data.get("servers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|server| {
            let status = server.get("status").and_then(Value::as_str)?;
            let name = server.get("name").and_then(Value::as_str)?;
            (status == "failed").then(|| mcp_unavailable(name, status))
        })
        .collect()
}

fn parse_message(expect: Expect, data: Option<&Value>) -> Vec<AgentItem> {
    let Some(text) = string_at(data, "content").filter(|text| !text.trim().is_empty()) else {
        return Vec::new();
    };
    let mut items = vec![AgentItem::Log(LogLine::Text(text.to_owned()))];
    if !requested_tools(data) {
        items.push(AgentItem::Result(message_result(expect, text)));
    }
    items
}

/// A message that precedes a tool call lists it in `toolRequests`; the final
/// answer's list is empty.
fn requested_tools(data: Option<&Value>) -> bool {
    data.and_then(|data| data.get("toolRequests"))
        .and_then(Value::as_array)
        .is_some_and(|requests| !requests.is_empty())
}

fn parse_tool_start(data: Option<&Value>) -> Vec<AgentItem> {
    let Some(data) = data else {
        return Vec::new();
    };
    let tool = string_at(Some(data), "toolName").unwrap_or_default();
    if let Some(server) = string_at(Some(data), "mcpServerName")
        .or_else(|| string_at(Some(data), "mcpConfigServerName"))
    {
        let name = string_at(Some(data), "mcpToolName").unwrap_or(tool);
        return vec![AgentItem::Log(LogLine::Tool {
            name: format!("mcp__{server}__{name}"),
            summary: String::new(),
        })];
    }
    if let Some(shell) = data.get("shellToolInfo") {
        let command = string_at(Some(shell), "displayCommand")
            .or_else(|| string_at(data.get("arguments"), "command"))
            .unwrap_or(tool);
        return vec![AgentItem::Log(LogLine::Tool {
            name: "exec".into(),
            summary: first_line(command),
        })];
    }
    if is_edit_tool(tool) {
        let path = edit_path(data.get("arguments")).unwrap_or(tool);
        return vec![AgentItem::Log(LogLine::Tool {
            name: "edit".into(),
            summary: first_line(path),
        })];
    }
    Vec::new()
}

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

/// The CLI writes the cause of a failed run to stderr, not to the stream, so the
/// reason names the exit code.
fn parse_result(expect: Expect, value: &Value) -> Vec<AgentItem> {
    let mut items = Vec::new();
    if let Some(requests) = value
        .pointer("/usage/premiumRequests")
        .and_then(Value::as_i64)
    {
        items.push(AgentItem::Log(LogLine::Other(format!(
            "Copilot reported {requests} premium request(s)"
        ))));
    }
    let code = value.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        items.push(AgentItem::Failure(failed_event(
            expect,
            format!("the Copilot run failed: the CLI exited with code {code}"),
        )));
    }
    items
}

/// `totalNanoAiu` is in billionths of an AI credit, and `tokenDetails` holds one
/// count per category.
fn checkpoint_log(data: Option<&Value>) -> Option<AgentItem> {
    let data = data?;
    #[allow(clippy::cast_precision_loss)]
    let credits = data
        .get("totalNanoAiu")
        .and_then(Value::as_i64)
        .unwrap_or(0) as f64
        / 1e9;
    let tokens = |key: &str| {
        data.pointer(&format!(
            "/accountingSnapshot/tokenDetails/{key}/tokenCount"
        ))
        .and_then(Value::as_i64)
        .unwrap_or(0)
    };
    Some(AgentItem::Log(LogLine::Other(format!(
        "Copilot usage: {} premium request(s), {credits:.4} AI credits; tokens: {} input, {} cache read, {} cache write, {} output",
        data.get("totalPremiumRequests")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        tokens("input"),
        tokens("cache_read"),
        tokens("cache_write"),
        tokens("output"),
    ))))
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
