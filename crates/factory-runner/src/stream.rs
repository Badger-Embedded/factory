use factory_core::PlanUsage;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

/// One line of `claude --output-format stream-json`, reduced to what the
/// factory keeps.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    Log(LogLine),
    Result(ResultMessage),
    /// The `system`/`init` line, which lists the MCP servers Claude Code tried
    /// to connect to and their status. The factory warns about each active
    /// server that is not `connected`.
    Init(Vec<McpServerStatus>),
    /// A `rate_limit_event` line: the plan windows Claude Code read from the
    /// API's response headers. A line that reports no window the tile shows is
    /// dropped.
    PlanUsage(PlanUsage),
}

/// One entry of the `mcp_servers` array in the `system`/`init` line: the server
/// name, which is the key the factory set in the run's MCP config, and the
/// connection status Claude Code reported, such as `connected` or `failed`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    #[serde(default)]
    pub status: String,
}

/// A line for the job's live log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    /// Text the agent wrote.
    Text(String),
    /// A tool call, with a short summary of its input.
    Tool { name: String, summary: String },
    /// A tool call whose result was an error. `input` is the call's input as
    /// JSON, with long strings cut. It is set only for `StructuredOutput`,
    /// whose rejected input explains the error.
    ToolError {
        name: String,
        input: Option<String>,
        error: String,
    },
    /// A line that is not stream-json, such as podman pulling the image.
    Other(String),
}

/// The final `result` message of a run.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResultMessage {
    pub subtype: String,
    #[serde(default)]
    pub is_error: bool,
    pub session_id: Option<String>,
    pub result: Option<String>,
    pub structured_output: Option<Value>,
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default, deserialize_with = "blocks")]
    content: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    ToolUse {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Value,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(other)]
    Other,
}

/// A user message's `content` may be a plain string, which holds no block.
fn blocks<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Block>, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Array(items) => Ok(items
            .into_iter()
            .map(|item| serde_json::from_value(item).unwrap_or(Block::Other))
            .collect()),
        _ => Ok(Vec::new()),
    }
}

const SUMMARY_KEYS: [&str; 6] = [
    "command",
    "file_path",
    "pattern",
    "path",
    "url",
    "description",
];
const MAX_SUMMARY: usize = 200;
const MAX_ERROR: usize = 1000;
const STRUCTURED_OUTPUT: &str = "StructuredOutput";

/// Parser for one run's stdout. It remembers each tool call until its result
/// arrives, so a failed result is logged with the tool's name and input.
#[derive(Default)]
pub struct ClaudeStream {
    calls: HashMap<String, (String, Value)>,
    key: Option<String>,
}

impl std::fmt::Debug for ClaudeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeStream").finish_non_exhaustive()
    }
}

/// Parses one stdout line with no memory of earlier lines, so a tool result
/// is not matched to its call. See [`ClaudeStream::parse_line`].
#[must_use]
pub fn parse_line(line: &str) -> Vec<StreamItem> {
    ClaudeStream::default().parse_line(line)
}

impl ClaudeStream {
    /// A parser that replaces `key` in a failed tool call's input and error
    /// before it cuts them, so a cut cannot leave a prefix of the key.
    #[must_use]
    pub fn with_key(key: Option<&str>) -> Self {
        ClaudeStream {
            calls: HashMap::new(),
            key: key.filter(|key| !key.is_empty()).map(str::to_owned),
        }
    }

    /// Parses one stdout line. Returns the log lines and, for the final line,
    /// the result. An assistant message can hold several blocks, so one line
    /// can give several log lines.
    #[must_use]
    pub fn parse_line(&mut self, line: &str) -> Vec<StreamItem> {
        parse_with(&mut self.calls, self.key.as_deref(), line)
    }
}

/// The `message.id` of an `assistant` line, which on `OpenRouter` is the
/// generation id its cost is read by. One message spans several lines with the
/// same id, so the caller keeps each once. Claude Code's own synthetic messages
/// carry no generation and give nothing.
#[must_use]
pub fn generation_id(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line.trim()).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = value.get("message")?;
    if message.get("model").and_then(Value::as_str) == Some("<synthetic>") {
        return None;
    }
    message
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn parse_with(
    calls: &mut HashMap<String, (String, Value)>,
    key: Option<&str>,
    line: &str,
) -> Vec<StreamItem> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return vec![StreamItem::Log(LogLine::Other(line.to_owned()))];
    };
    if value.get("type").and_then(Value::as_str) == Some("result") {
        return match serde_json::from_value(value) {
            Ok(result) => vec![StreamItem::Result(result)],
            Err(_) => vec![StreamItem::Log(LogLine::Other(line.to_owned()))],
        };
    }
    if value.get("type").and_then(Value::as_str) == Some("rate_limit_event") {
        return value
            .get("rate_limit_info")
            .and_then(|info| PlanUsage::from_rate_limit_info(info.clone()).ok())
            .map(StreamItem::PlanUsage)
            .into_iter()
            .collect();
    }
    if value.get("type").and_then(Value::as_str) == Some("system") {
        if value.get("subtype").and_then(Value::as_str) == Some("init") {
            let servers = value
                .get("mcp_servers")
                .and_then(|servers| serde_json::from_value(servers.clone()).ok())
                .unwrap_or_default();
            return vec![StreamItem::Init(servers)];
        }
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_value::<Line>(value) else {
        return Vec::new();
    };
    if parsed.kind != "assistant" && parsed.kind != "user" {
        return Vec::new();
    }
    parsed
        .message
        .map(|message| message.content)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|block| match block {
            Block::Text { text } if parsed.kind == "assistant" && !text.trim().is_empty() => {
                Some(StreamItem::Log(LogLine::Text(text)))
            }
            Block::ToolUse { id, name, input } if parsed.kind == "assistant" => {
                let summary = summarize(&input);
                if let Some(id) = id {
                    calls.insert(id, (name.clone(), input));
                }
                Some(StreamItem::Log(LogLine::Tool { name, summary }))
            }
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let call = calls.remove(&tool_use_id);
                if !is_error {
                    return None;
                }
                let (name, input) = call.unwrap_or_else(|| ("tool".to_owned(), Value::Null));
                let input = (name == STRUCTURED_OUTPUT).then(|| cut_json(&input, key));
                Some(StreamItem::Log(LogLine::ToolError {
                    name,
                    input,
                    error: cut(&redact(&result_text(&content), key), MAX_ERROR),
                }))
            }
            _ => None,
        })
        .collect()
}

/// A result's `content` is a string or a list of text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>())
    } else {
        text.to_owned()
    }
}

fn redact(text: &str, key: Option<&str>) -> String {
    match key {
        Some(key) => text.replace(key, "[redacted]"),
        None => text.to_owned(),
    }
}

fn cut_json(input: &Value, key: Option<&str>) -> String {
    fn shorten(value: &Value, key: Option<&str>) -> Value {
        match value {
            Value::String(text) => Value::String(cut(&redact(text, key), MAX_SUMMARY)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| shorten(item, key)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(name, value)| (redact(name, key), shorten(value, key)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    serde_json::to_string(&shorten(input, key)).unwrap_or_default()
}

fn summarize(input: &Value) -> String {
    let text = SUMMARY_KEYS
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .unwrap_or_default();
    let first = text.lines().next().unwrap_or_default();
    if first.chars().count() > MAX_SUMMARY {
        format!("{}…", first.chars().take(MAX_SUMMARY).collect::<String>())
    } else {
        first.to_owned()
    }
}
