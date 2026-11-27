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

use factory_core::{Agent, Cli};

use crate::agent::{
    AgentBackend, AgentItem, Credential, PromptStyle, RunResult, TokenUsage, write_replacing,
};
use crate::outcome::{Expect, event_from_result};
use crate::stream::{ClaudeStream, StreamItem, generation_id, parse_line};
use crate::worker::{
    AGENTS_DIR, McpConfig, McpServerEntry, McpUnavailable, ProviderKind,
    STDIO_STARTUP_TIMEOUT_SECS, SessionRun, WorkerConfig, WorkerRun,
};

/// The Claude Code config directory inside the worker, mounted from the job's
/// `claude` directory on the host. It keeps the session files `--resume` reads.
const CONFIG_DIR: &str = "/claude";
const MCP_CONFIG_FILE: &str = "factory-mcp.json";

/// The variable Claude Code reads a Local server's key from. A run with no key
/// passes [`LOCAL_PLACEHOLDER_KEY`] by value instead, so the CLI does not ask
/// for a login.
const LOCAL_KEY_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
const LOCAL_PLACEHOLDER_KEY: &str = "local";

/// Claude Code's own request timeout, 600000 ms unless set. A Local run raises
/// it to at least the model load timeout, because the server answers the first
/// request only after it loads the model.
const DEFAULT_API_TIMEOUT_MS: u128 = 600_000;

/// Settings for every Claude run. Claude Code adds a `Co-Authored-By` trailer
/// to commits and a footer to pull request descriptions unless `attribution`
/// empties them, and GitHub copies a branch's co-author trailers into the
/// squash commit on the base branch. Empty strings work on every Claude Code
/// version; `"attribution": false` makes versions before 2.1.281 skip the whole
/// settings value.
const AGENT_SETTINGS: &str = r#"{"attribution":{"commit":"","pr":"","sessionUrl":false}}"#;

/// The review-method paragraph for a Claude run. Claude loads the factory's
/// review skill through `--plugin-dir`, so the wording names the skill rather
/// than a path.
const REVIEW_METHOD: &str = "The factory's review skill describes the method. The repository you \
     are reviewing may carry its own review, lint or project skills; use them, since they know \
     rules this prompt does not.\n\n";

/// Claude Code, run with the `claude -p` CLI in the worker.
pub struct ClaudeAgent;

impl AgentBackend for ClaudeAgent {
    fn credential(&self) -> Credential {
        Credential::Env("CLAUDE_CODE_OAUTH_TOKEN")
    }

    fn prompt_style(&self) -> PromptStyle {
        PromptStyle {
            rules_doc: Cli::ClaudeCode.rules_doc(),
            review_method: REVIEW_METHOD,
            schema_in_prompt: None,
        }
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

    fn provider_host_config_dir(&self, kind: &ProviderKind) -> &'static str {
        match kind {
            ProviderKind::Local { .. } => "local-claude",
            ProviderKind::OpenRouter => "openrouter-claude",
        }
    }

    fn secret_env<'a>(&self, run: &'a WorkerRun) -> Vec<(&'static str, &'a str)> {
        match run.provider.as_ref().and_then(|local| local.key.as_deref()) {
            Some(key) => vec![(LOCAL_KEY_ENV, key)],
            None => Vec::new(),
        }
    }

    fn provider_env(&self, run: &WorkerRun) -> Vec<(&'static str, String)> {
        let Some(local) = run.provider.as_ref() else {
            return Vec::new();
        };
        let model = run.model.clone().unwrap_or_default();
        let ProviderKind::Local { load_timeout } = local.kind else {
            // OpenRouter answers at once, so the timeouts stay at Claude's own.
            // The empty `ANTHROPIC_API_KEY` keeps Claude from sending a key of
            // its own beside the auth token.
            return vec![
                ("ANTHROPIC_BASE_URL", local.base_url.clone()),
                ("ANTHROPIC_API_KEY", String::new()),
                ("ANTHROPIC_DEFAULT_HAIKU_MODEL", model.clone()),
                ("ANTHROPIC_SMALL_FAST_MODEL", model),
                ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
            ];
        };
        let load_ms = load_timeout.as_millis();
        let mut env = vec![
            ("ANTHROPIC_BASE_URL", local.base_url.clone()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", model.clone()),
            ("ANTHROPIC_SMALL_FAST_MODEL", model),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
            (
                "API_TIMEOUT_MS",
                load_ms.max(DEFAULT_API_TIMEOUT_MS).to_string(),
            ),
            (
                "CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS",
                load_ms.max(DEFAULT_API_TIMEOUT_MS).to_string(),
            ),
        ];
        if local.key.is_none() {
            env.push((LOCAL_KEY_ENV, LOCAL_PLACEHOLDER_KEY.to_owned()));
        }
        env
    }

    fn prepare(&self, run: &WorkerRun) -> std::io::Result<()> {
        match run.mcp.as_ref().filter(|mcp| mcp.has_stdio()) {
            Some(mcp) => write_replacing(&run.config_dir, MCP_CONFIG_FILE, &mcp_config(mcp)),
            None => Ok(()),
        }
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
            settings(run.mcp.as_ref()),
            session_flag.into(),
            session_id.clone(),
        ];
        if config.agents_dir.is_some() {
            claude.push("--plugin-dir".into());
            claude.push(AGENTS_DIR.into());
        }
        if let Some(model) = run.model.as_deref().filter(|model| !model.is_empty()) {
            claude.push("--model".into());
            claude.push(model.into());
        }
        claude.push("--effort".into());
        let default_effort = run
            .provider
            .as_ref()
            .map_or(Agent::Claude, |provider| provider.kind.agent())
            .default_effort();
        claude.push(run.effort.as_deref().unwrap_or(default_effort).into());
        // One `http` entry per active MCP server, pointed at the proxy with the
        // run token. A run with no active server adds nothing, so its arguments
        // match a run from before MCP. A run with a stdio server reads the config
        // from a file, since its arguments can outgrow one argv element.
        if let Some(mcp) = run.mcp.as_ref().filter(|mcp| !mcp.servers.is_empty()) {
            claude.push("--mcp-config".into());
            if mcp.has_stdio() {
                claude.push(format!("{CONFIG_DIR}/{MCP_CONFIG_FILE}"));
            } else {
                claude.push(mcp_config(mcp));
            }
        }
        claude.into_iter().map(OsString::from).collect()
    }

    fn stream(
        &self,
        expect: Expect,
        provider: Option<ProviderKind>,
        key: Option<&str>,
    ) -> crate::agent::LineParser<'_> {
        let mut stream = ClaudeStream::with_key(key);
        Box::new(move |line| {
            let mut items = convert(expect, stream.parse_line(line));
            if provider == Some(ProviderKind::OpenRouter) {
                items.extend(generation_id(line).map(AgentItem::Generation));
            }
            if provider.is_some() {
                append_local_token_usage(items, line)
            } else {
                items
            }
        })
    }

    fn parse_provider(&self, expect: Expect, line: &str) -> Vec<AgentItem> {
        let items = self.parse(expect, line);
        append_local_token_usage(items, line)
    }

    fn parse(&self, expect: Expect, line: &str) -> Vec<AgentItem> {
        convert(expect, parse_line(line))
    }
}

fn append_local_token_usage(mut items: Vec<AgentItem>, line: &str) -> Vec<AgentItem> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim())
        && value.get("type").and_then(serde_json::Value::as_str) == Some("result")
        && let Some(usage) = value.get("usage").filter(|usage| usage.is_object())
    {
        let count = |key: &str| {
            usage
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        let counted = ["input_tokens", "cache_read_input_tokens", "output_tokens"]
            .iter()
            .any(|key| usage.get(*key).is_some_and(serde_json::Value::is_u64));
        if !counted {
            return items;
        }
        items.push(AgentItem::UsageTotal(TokenUsage {
            input_tokens: count("input_tokens"),
            cached_input_tokens: count("cache_read_input_tokens"),
            output_tokens: count("output_tokens"),
            reasoning_output_tokens: 0,
        }));
    }
    items
}

fn convert(expect: Expect, items: Vec<StreamItem>) -> Vec<AgentItem> {
    items
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

/// The JSON for `claude --settings`. A run with a stdio server raises Claude's
/// MCP startup timeout, `MCP_TIMEOUT`, through the settings `env` key, because a
/// first start of a package runner downloads the package and the default is 30
/// seconds. Settings carry it, so the container gets no `--env` for it.
fn settings(mcp: Option<&McpConfig>) -> String {
    if mcp.is_some_and(McpConfig::has_stdio) {
        serde_json::json!({
            "attribution": { "commit": "", "pr": "", "sessionUrl": false },
            "env": { "MCP_TIMEOUT": (STDIO_STARTUP_TIMEOUT_SECS * 1000).to_string() },
        })
        .to_string()
    } else {
        AGENT_SETTINGS.to_owned()
    }
}

/// The JSON for `claude --mcp-config`: one entry per active server. A remote
/// server is an `http` entry pointing at the proxy and carrying
/// `Authorization: Bearer <token>` as its only header; no stored credential
/// appears here, the proxy adds it. A stdio server is a `stdio` entry with its
/// command and arguments.
fn mcp_config(mcp: &McpConfig) -> String {
    let servers: serde_json::Map<String, serde_json::Value> = mcp
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
                        "headers": { "Authorization": format!("Bearer {}", proxy.token) },
                    }),
                ))
            }
            McpServerEntry::Stdio {
                name,
                command,
                args,
            } => Some((
                name.clone(),
                serde_json::json!({ "type": "stdio", "command": command, "args": args }),
            )),
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
