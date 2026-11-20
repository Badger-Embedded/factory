use serde::Deserialize;
use serde_json::Value;

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
    #[serde(default)]
    content: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(other)]
    Other,
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

/// Parses one stdout line. Returns the log lines and, for the final line,
/// the result. An assistant message can hold several blocks, so one line can
/// give several log lines.
#[must_use]
pub fn parse_line(line: &str) -> Vec<StreamItem> {
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
    if parsed.kind != "assistant" {
        return Vec::new();
    }
    parsed
        .message
        .map(|message| message.content)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|block| match block {
            Block::Text { text } if !text.trim().is_empty() => {
                Some(StreamItem::Log(LogLine::Text(text)))
            }
            Block::ToolUse { name, input } => Some(StreamItem::Log(LogLine::Tool {
                name,
                summary: summarize(&input),
            })),
            _ => None,
        })
        .collect()
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
