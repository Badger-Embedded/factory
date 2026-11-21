//! The Claude Code agent: its implementation of [`AgentBackend`].
//!
//! Everything specific to running a stage with the Claude Code CLI lives here:
//! the `claude -p` command line, the `CLAUDE_CODE_OAUTH_TOKEN` credential, the
//! `CLAUDE_CONFIG_DIR` config directory, the session-resume check, and the
//! stream-json parser in [`stream`](crate::stream). See the design doc,
//! "Running Claude Code in a worker".

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use factory_core::Agent;

use crate::agent::{AgentBackend, AgentItem, RunResult};
use crate::outcome::{Expect, event_from_result};
use crate::stream::{StreamItem, parse_line};
use crate::worker::{McpConfig, McpUnavailable, PLUGIN_DIR, SessionRun, WorkerConfig, WorkerRun};

/// The Claude Code config directory inside the worker, mounted from the job's
/// `claude` directory on the host. It keeps the session files `--resume` reads.
const CONFIG_DIR: &str = "/claude";

/// Settings for every Claude run. Claude Code adds a `Co-Authored-By` trailer
/// to commits and a footer to pull request descriptions unless `attribution`
/// empties them, and GitHub copies a branch's co-author trailers into the
/// squash commit on the base branch. Empty strings work on every Claude Code
/// version; `"attribution": false` makes versions before 2.1.281 skip the whole
/// settings value.
const AGENT_SETTINGS: &str = r#"{"attribution":{"commit":"","pr":"","sessionUrl":false}}"#;

/// Claude Code, run with the `claude -p` CLI in the worker.
pub struct ClaudeAgent;

impl AgentBackend for ClaudeAgent {
    fn credential_env(&self) -> &'static str {
        "CLAUDE_CODE_OAUTH_TOKEN"
    }

    fn config_dir_env(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn config_dir(&self) -> &'static str {
        CONFIG_DIR
    }

    fn host_config_dir(&self) -> &'static str {
        "claude"
    }

    fn session_for(&self, host_config_dir: &Path, id: &str) -> SessionRun {
        let transcript = format!("{id}.jsonl");
        if contains_file(&host_config_dir.join("projects"), &transcript) {
            SessionRun::Resume(id.to_owned())
        } else {
            SessionRun::Start(id.to_owned())
        }
    }

    fn command(&self, config: &WorkerConfig, run: &WorkerRun) -> Vec<OsString> {
        let (session_flag, session_id) = match &run.session {
            SessionRun::Start(id) => ("--session-id", id),
            SessionRun::Resume(id) => ("--resume", id),
        };
        let mut claude: Vec<String> = vec![
            "claude".into(),
            "-p".into(),
            run.prompt.clone(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "bypassPermissions".into(),
            "--json-schema".into(),
            run.expect.schema().to_string(),
            "--settings".into(),
            AGENT_SETTINGS.into(),
            session_flag.into(),
            session_id.clone(),
        ];
        if config.plugin_dir.is_some() {
            claude.push("--plugin-dir".into());
            claude.push(PLUGIN_DIR.into());
        }
        if let Some(model) = run.model.as_deref().filter(|model| !model.is_empty()) {
            claude.push("--model".into());
            claude.push(model.into());
        }
        claude.push("--effort".into());
        claude.push(
            run.effort
                .as_deref()
                .unwrap_or_else(|| Agent::Claude.default_effort())
                .into(),
        );
        // One `http` entry per active MCP server, pointed at the proxy with the
        // run token. A run with no active server adds nothing, so its arguments
        // match a run from before MCP.
        if let Some(mcp) = run.mcp.as_ref().filter(|mcp| !mcp.servers.is_empty()) {
            claude.push("--mcp-config".into());
            claude.push(mcp_config(mcp));
        }
        claude.into_iter().map(OsString::from).collect()
    }

    fn parse(&self, expect: Expect, line: &str) -> Vec<AgentItem> {
        parse_line(line)
            .into_iter()
            .flat_map(|item| match item {
                StreamItem::Log(line) => vec![AgentItem::Log(line)],
                StreamItem::PlanUsage(usage) => vec![AgentItem::PlanUsage(usage)],
                StreamItem::Result(message) => vec![AgentItem::Result(RunResult {
                    event: event_from_result(expect, &message),
                    session_id: message.session_id.clone(),
                    cost_usd: message.total_cost_usd,
                })],
                StreamItem::Init(servers) => servers
                    .into_iter()
                    .filter(|server| server.status != "connected")
                    .map(|server| {
                        AgentItem::Unavailable(McpUnavailable {
                            reason: format!(
                                "the agent reported it as {}",
                                status_label(&server.status)
                            ),
                            server: server.name,
                        })
                    })
                    .collect(),
            })
            .collect()
    }
}

/// The JSON for `claude --mcp-config`: one `http` entry per active server, each
/// pointing at the proxy and carrying `Authorization: Bearer <token>` as its
/// only header. No stored credential appears here; the proxy adds it.
fn mcp_config(mcp: &McpConfig) -> String {
    let servers: serde_json::Map<String, serde_json::Value> = mcp
        .servers
        .iter()
        .map(|name| {
            (
                name.clone(),
                serde_json::json!({
                    "type": "http",
                    "url": format!("{}/mcp/{name}", mcp.proxy_url),
                    "headers": { "Authorization": format!("Bearer {}", mcp.token) },
                }),
            )
        })
        .collect();
    serde_json::json!({ "mcpServers": servers }).to_string()
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
