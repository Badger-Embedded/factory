//! The Codex backend: its command line, its secrecy, its strict schema and its
//! JSON Lines parser. The parser tests read fixtures captured in the shape of
//! `codex exec --json` at the pinned `rust-v0.165.0`; see `codex_image.rs` for
//! the image side.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_core::{Agent, AgentResult, Cli, Event};
use factory_runner::{
    AgentItem, Expect, LocalTarget, LogLine, McpConfig, McpProxy, McpServerEntry, RunResult,
    SessionRun, TokenUsage, WorkerConfig, WorkerRun, backend, run,
};
use serde_json::Value;

const THREAD: &str = "019abd3c-1f2a-7bc4-9d6e-2a1b3c4d5e6f";
/// A subscription `auth.json`, shaped like the one `codex login` writes. Only
/// its bytes matter here: the backend never parses it.
const AUTH: &str = r#"{"tokens":{"access_token":"at-1","refresh_token":"rt-1"},"last_refresh":"2026-11-20T00:00:00Z"}"#;

fn codex_run(dir: &Path, session: SessionRun) -> WorkerRun {
    WorkerRun {
        name: "factory-job-7-run-1".into(),
        agent: Agent::Codex,
        workspace: dir.join("work"),
        config_dir: dir.join("codex"),
        session,
        token: AUTH.into(),
        expect: Expect::Spec,
        prompt: "Write the spec.".into(),
        model: None,
        effort: None,
        mcp: None,
        packages: Vec::new(),
        local: None,
    }
}

fn strings(args: &[std::ffi::OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let index = args.iter().position(|arg| arg == flag).unwrap();
    &args[index + 1]
}

/// Every value passed after an `--env` flag.
fn env_names(args: &[String]) -> Vec<&str> {
    args.iter()
        .enumerate()
        .filter(|(_, arg)| *arg == "--env")
        .map(|(index, _)| args[index + 1].as_str())
        .collect()
}

fn codex_args(run: &WorkerRun) -> Vec<String> {
    strings(&factory_runner::podman_args(&WorkerConfig::default(), run))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn a_new_session_runs_codex_exec_with_the_schema_bypass_and_effort() {
    let dir = tempfile::tempdir().unwrap();
    let args = codex_args(&codex_run(dir.path(), SessionRun::Start(THREAD.into())));

    // The agent invocation sits after the image.
    let codex = args.iter().position(|arg| arg == "codex").unwrap();
    assert_eq!(args[codex + 1], "exec");
    assert!(!args.iter().any(|arg| arg == "resume"), "a new session");
    assert!(args.iter().any(|arg| arg == "--json"));
    assert!(
        args.iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
        "the container is the sandbox"
    );
    assert_eq!(
        value_after(&args, "--output-schema"),
        "/codex/output-schema.json"
    );
    assert!(
        args.iter()
            .any(|arg| arg == "model_reasoning_effort=medium"),
        "the default effort is passed as a config override: {args:?}"
    );
    // CODEX_HOME points Codex at the mounted config directory.
    assert!(args.iter().any(|arg| arg == "CODEX_HOME=/codex"));
    assert!(
        args.iter()
            .any(|arg| *arg == format!("{}/codex:/codex", dir.path().display())),
        "the config directory is mounted at CODEX_HOME: {args:?}"
    );
    // The prompt is the last argument.
    assert_eq!(args.last().unwrap(), "Write the spec.");
}

#[test]
fn a_resumed_session_names_the_thread_id() {
    let dir = tempfile::tempdir().unwrap();
    let args = codex_args(&codex_run(dir.path(), SessionRun::Resume(THREAD.into())));

    let exec = args.iter().position(|arg| arg == "exec").unwrap();
    assert_eq!(args[exec + 1], "resume");
    assert_eq!(args[exec + 2], THREAD);
    assert!(args.iter().any(|arg| arg == "--json"));
}

#[test]
fn an_empty_model_passes_no_model_flag_and_a_set_one_is_passed() {
    let dir = tempfile::tempdir().unwrap();
    for model in [None, Some("")] {
        let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
        run.model = model.map(str::to_owned);
        let args = codex_args(&run);
        assert!(!args.iter().any(|arg| arg == "--model"), "{args:?}");
    }
    let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
    run.model = Some("gpt-5.1-codex".into());
    assert_eq!(value_after(&codex_args(&run), "--model"), "gpt-5.1-codex");
}

#[test]
fn each_codex_effort_is_passed_as_a_config_override() {
    let dir = tempfile::tempdir().unwrap();
    for effort in Agent::Codex.efforts() {
        let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
        run.effort = Some((*effort).to_owned());
        let args = codex_args(&run);
        assert!(
            args.iter()
                .any(|arg| *arg == format!("model_reasoning_effort={effort}")),
            "{effort}: {args:?}"
        );
    }
}

#[test]
fn the_mcp_token_passes_by_name_only_and_the_login_never_enters_the_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let mut run = codex_run(dir.path(), SessionRun::Resume(THREAD.into()));
    run.mcp = Some(McpConfig {
        proxy: Some(McpProxy {
            url: "http://factory:8081".into(),
            token: "d34db33f-run-token".into(),
        }),
        servers: ["context7", "docs"]
            .map(|name| McpServerEntry::Remote { name: name.into() })
            .to_vec(),
    });
    let args = codex_args(&run);

    // Each active server becomes a config override pointed at the proxy, and the
    // token is named, not spelled out.
    for server in ["context7", "docs"] {
        assert!(
            args.iter().any(|arg| *arg
                == format!("mcp_servers.{server}.url=\"http://factory:8081/mcp/{server}\"")),
            "missing url override for {server}: {args:?}"
        );
        assert!(
            args.iter().any(|arg| *arg
                == format!("mcp_servers.{server}.bearer_token_env_var=\"FACTORY_MCP_TOKEN\"")),
            "missing bearer env override for {server}: {args:?}"
        );
    }
    // The run token reaches the container by name only.
    assert!(env_names(&args).contains(&"FACTORY_MCP_TOKEN"));
    assert!(
        !args.iter().any(|arg| arg.contains("d34db33f-run-token")),
        "the MCP run token must not appear in the arguments: {args:?}"
    );
    // The login is a file credential, so no part of it, and no credential
    // `--env`, appears.
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("rt-1") || arg.contains("at-1")),
        "no part of auth.json may appear in the arguments: {args:?}"
    );
    assert!(
        !env_names(&args).contains(&"CODEX_ACCESS_TOKEN"),
        "a subscription login passes no credential env"
    );
}

fn stdio_entry() -> McpServerEntry {
    McpServerEntry::Stdio {
        name: "pw".into(),
        command: "playwright-mcp".into(),
        args: ["--no-sandbox", "a \"b\\c\" d"].map(String::from).to_vec(),
    }
}

fn written_config(run: &WorkerRun, dir: &Path) -> toml::Value {
    backend(Agent::Codex).unwrap().prepare(run).unwrap();
    toml::from_str(&fs::read_to_string(dir.join("codex/config.toml")).unwrap()).unwrap()
}

#[test]
fn a_stdio_server_goes_into_config_toml_with_a_startup_timeout_and_no_token() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("codex")).unwrap();
    let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
    let plain = codex_args(&run);
    run.mcp = Some(McpConfig {
        proxy: None,
        servers: vec![stdio_entry()],
    });
    let args = codex_args(&run);

    assert_eq!(args, plain);
    // The quote, backslash and space in the second argument are escaped, so
    // TOML reads them back unchanged.
    let config = written_config(&run, dir.path());
    let server = &config["mcp_servers"]["pw"];
    assert_eq!(server["command"].as_str(), Some("playwright-mcp"));
    assert_eq!(
        server["args"],
        toml::Value::try_from(["--no-sandbox", "a \"b\\c\" d"]).unwrap()
    );
    assert_eq!(server["startup_timeout_sec"].as_integer(), Some(120));
    assert_eq!(config["cli_auth_credentials_store"].as_str(), Some("file"));
    assert!(!env_names(&args).contains(&"FACTORY_MCP_TOKEN"), "{args:?}");
}

#[test]
fn stdio_arguments_too_big_for_one_argv_element_go_through_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("codex")).unwrap();
    let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
    let big: Vec<String> = (0..64).map(|i| format!("{i:x<4096}")).collect();
    run.mcp = Some(McpConfig {
        proxy: None,
        servers: vec![McpServerEntry::Stdio {
            name: "pw".into(),
            command: "playwright-mcp".into(),
            args: big.clone(),
        }],
    });
    // Linux rejects one argv element of 128 KiB or more.
    assert!(codex_args(&run).iter().all(|arg| arg.len() < 128 * 1024));
    let config = written_config(&run, dir.path());
    assert_eq!(
        config["mcp_servers"]["pw"]["args"],
        toml::Value::try_from(big).unwrap()
    );
}

#[test]
fn a_remote_and_a_stdio_server_share_a_codex_run() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("codex")).unwrap();
    let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
    run.mcp = Some(McpConfig {
        proxy: Some(McpProxy {
            url: "http://factory:8081".into(),
            token: "tok".into(),
        }),
        servers: vec![
            McpServerEntry::Remote {
                name: "context7".into(),
            },
            stdio_entry(),
        ],
    });
    let args = codex_args(&run);
    assert!(
        args.iter()
            .any(|arg| { arg == "mcp_servers.context7.url=\"http://factory:8081/mcp/context7\"" })
    );
    assert!(!args.iter().any(|arg| arg.starts_with("mcp_servers.pw.")));
    let config = written_config(&run, dir.path());
    assert_eq!(
        config["mcp_servers"]["pw"]["command"].as_str(),
        Some("playwright-mcp")
    );
    assert!(env_names(&args).contains(&"FACTORY_MCP_TOKEN"));
}

#[test]
fn a_stdio_tool_call_logs_the_namespaced_tool_name() {
    let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"pw","tool":"browser_navigate","status":"completed"}}"#;
    let items = backend(Agent::Codex).unwrap().parse(Expect::Spec, line);
    assert!(
        items.iter().any(|item| matches!(
            item,
            AgentItem::Log(LogLine::Tool { name, .. }) if name == "mcp__pw__browser_navigate"
        )),
        "{items:?}"
    );
}

#[test]
fn a_run_with_no_active_server_passes_no_mcp_flags_or_token() {
    let dir = tempfile::tempdir().unwrap();
    for mcp in [None, Some(Vec::new())] {
        let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
        run.mcp = mcp.map(|servers| McpConfig {
            proxy: Some(McpProxy {
                url: "http://factory:8081".into(),
                token: "unused".into(),
            }),
            servers,
        });
        let args = codex_args(&run);
        assert!(
            !args.iter().any(|arg| arg.starts_with("mcp_servers.")),
            "{args:?}"
        );
        assert!(!env_names(&args).contains(&"FACTORY_MCP_TOKEN"), "{args:?}");
    }
}

#[test]
fn a_claude_run_is_unchanged_by_the_codex_backend() {
    // Claude keeps its environment credential and carries its MCP token in the
    // config argument, not an env var, so the Codex change touches none of it.
    let dir = tempfile::tempdir().unwrap();
    let mut run = WorkerRun {
        name: "factory-job-7-run-1".into(),
        agent: Agent::Claude,
        workspace: dir.path().join("work"),
        config_dir: dir.path().join("claude"),
        session: SessionRun::Start(THREAD.into()),
        token: "sk-ant-oat01-token".into(),
        expect: Expect::Spec,
        prompt: "Write the spec.".into(),
        model: None,
        effort: None,
        mcp: Some(McpConfig {
            proxy: Some(McpProxy {
                url: "http://factory:8081".into(),
                token: "run-token".into(),
            }),
            servers: vec![McpServerEntry::Remote {
                name: "context7".into(),
            }],
        }),
        packages: Vec::new(),
        local: None,
    };
    let args = strings(&factory_runner::podman_args(&WorkerConfig::default(), &run));

    assert!(env_names(&args).contains(&"CLAUDE_CODE_OAUTH_TOKEN"));
    assert!(
        !env_names(&args).contains(&"FACTORY_MCP_TOKEN"),
        "Claude's MCP token stays in the config argument"
    );
    let config: Value = serde_json::from_str(value_after(&args, "--mcp-config")).unwrap();
    assert_eq!(
        config["mcpServers"]["context7"]["headers"]["Authorization"],
        "Bearer run-token"
    );
    run.mcp = None;
    assert!(
        strings(&factory_runner::podman_args(&WorkerConfig::default(), &run))
            .iter()
            .any(|arg| arg == "claude"),
        "a Claude run still runs claude"
    );
}

#[test]
fn every_codex_schema_variant_is_strict() {
    for expect in [
        Expect::Clarify,
        Expect::Spec,
        Expect::Plan,
        Expect::Commits,
        Expect::FixCi,
        Expect::Review,
    ] {
        let schema = expect.strict_schema();
        assert_objects_strict(&schema, expect);
    }
}

/// Every object in the schema lists each of its properties in `required` and
/// forbids extra keys, as the strict structured output format demands.
fn assert_objects_strict(value: &Value, expect: Expect) {
    if value.get("type").and_then(type_has_object).unwrap_or(false) {
        let properties = value["properties"].as_object().unwrap();
        let required: Vec<&str> = value["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap())
            .collect();
        for key in properties.keys() {
            assert!(
                required.contains(&key.as_str()),
                "{expect:?}: {key} is not required"
            );
        }
        assert_eq!(
            value["additionalProperties"],
            Value::Bool(false),
            "{expect:?}: additionalProperties is not false"
        );
        for property in properties.values() {
            assert_objects_strict(property, expect);
        }
    }
    if let Some(items) = value.get("items") {
        assert_objects_strict(items, expect);
    }
}

/// Whether a `type` value, a string or an array of them, includes `"object"`.
fn type_has_object(ty: &Value) -> Option<bool> {
    match ty {
        Value::String(name) => Some(name == "object"),
        Value::Array(names) => Some(names.iter().any(|name| name == "object")),
        _ => None,
    }
}

#[test]
fn a_null_optional_field_resolves_the_same_as_an_absent_one() {
    use factory_runner::event_from_output;
    // Spec: a null spec is as empty as an absent one; a null questions list is
    // as absent as none.
    let null = event_from_output(
        Expect::Spec,
        Some(serde_json::json!({"outcome": "spec", "questions": null, "spec": null})),
    );
    let absent = event_from_output(Expect::Spec, Some(serde_json::json!({"outcome": "spec"})));
    assert!(matches!(null, Event::AgentFailed { .. }));
    assert_eq!(format!("{null:?}"), format!("{absent:?}"));

    let null_questions = event_from_output(
        Expect::Clarify,
        Some(serde_json::json!({"outcome": "questions", "questions": null})),
    );
    assert!(matches!(null_questions, Event::AgentFailed { .. }));
}

#[test]
fn the_parser_reads_a_spec_run_into_logs_a_thread_id_usage_and_a_result() {
    let codex = backend(Agent::Codex).unwrap();
    let text = fs::read_to_string(fixture("codex-spec-run.jsonl")).unwrap();
    let items: Vec<AgentItem> = text
        .lines()
        .flat_map(|line| codex.parse(Expect::Spec, line))
        .collect();

    let logs: Vec<&LogLine> = items
        .iter()
        .filter_map(|item| match item {
            AgentItem::Log(line) => Some(line),
            _ => None,
        })
        .collect();
    assert_eq!(
        logs,
        [
            &LogLine::Tool {
                name: "exec".into(),
                summary: "rg -n reboot software".into(),
            },
            &LogLine::Tool {
                name: "mcp__context7__get-library-docs".into(),
                summary: String::new(),
            },
            &LogLine::Text(
                r#"{"outcome":"spec","questions":null,"spec":"Goal: reboot the DUT from its page."}"#
                    .into()
            ),
        ]
    );

    let session: Vec<&String> = items
        .iter()
        .filter_map(|item| match item {
            AgentItem::Session(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(session, [&THREAD.to_owned()]);

    let usage: Vec<&TokenUsage> = items
        .iter()
        .filter_map(|item| match item {
            AgentItem::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage,
        [&TokenUsage {
            input_tokens: 1200,
            cached_input_tokens: 400,
            output_tokens: 350,
            reasoning_output_tokens: 120,
        }]
    );

    let result = items
        .iter()
        .rev()
        .find_map(|item| match item {
            AgentItem::Result(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        result,
        &RunResult {
            event: Event::Agent(AgentResult::Spec(
                "Goal: reboot the DUT from its page.".into()
            )),
            session_id: None,
            cost_usd: None,
        }
    );
}

#[test]
fn the_parser_sums_usage_over_several_turns() {
    let codex = backend(Agent::Codex).unwrap();
    let turn = |input: i64| {
        format!(
            r#"{{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":10,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":1}}}}"#
        )
    };
    let mut total = TokenUsage::default();
    for line in [turn(100), turn(200)] {
        for item in codex.parse(Expect::Commits, &line) {
            if let AgentItem::Usage(usage) = item {
                total = total.plus(usage);
            }
        }
    }
    assert_eq!(
        total,
        TokenUsage {
            input_tokens: 300,
            cached_input_tokens: 20,
            output_tokens: 10,
            reasoning_output_tokens: 2,
        }
    );
}

#[test]
fn a_message_that_is_not_valid_json_fails_the_run() {
    let codex = backend(Agent::Codex).unwrap();
    let line = r#"{"type":"item.completed","item":{"id":"x","type":"agent_message","text":"Sorry, I could not do it."}}"#;
    let result = codex
        .parse(Expect::Spec, line)
        .into_iter()
        .find_map(|item| match item {
            AgentItem::Result(result) => Some(result),
            _ => None,
        })
        .unwrap();
    match result.event {
        Event::AgentFailed { reason } => assert!(reason.contains("not valid JSON"), "{reason}"),
        other => panic!("expected AgentFailed, got {other:?}"),
    }
}

#[test]
fn a_turn_failure_without_a_message_fails_the_run_and_a_review_fails_closed() {
    let codex = backend(Agent::Codex).unwrap();
    let text = fs::read_to_string(fixture("codex-exec-auth-failure.jsonl")).unwrap();

    // Outside review, the fatal error becomes an agent failure carrying Codex's
    // message, used because no agent message arrived.
    let failure = |expect: Expect| {
        text.lines()
            .flat_map(|line| codex.parse(expect, line))
            .filter_map(|item| match item {
                AgentItem::Failure(event) => Some(event),
                _ => None,
            })
            .last()
            .unwrap()
    };
    match failure(Expect::Commits) {
        Event::AgentFailed { reason } => assert!(reason.contains("401 Unauthorized"), "{reason}"),
        other => panic!("expected AgentFailed, got {other:?}"),
    }
    // A review run fails closed with the same cause.
    assert!(matches!(
        failure(Expect::Review),
        Event::ReviewFailed { .. }
    ));
}

#[test]
fn a_review_without_a_valid_verdict_fails_closed() {
    let codex = backend(Agent::Codex).unwrap();
    let text = fs::read_to_string(fixture("codex-review-no-verdict.jsonl")).unwrap();
    let result = text
        .lines()
        .flat_map(|line| codex.parse(Expect::Review, line))
        .filter_map(|item| match item {
            AgentItem::Result(result) => Some(result),
            _ => None,
        })
        .last()
        .unwrap();
    assert!(matches!(result.event, Event::ReviewFailed { .. }));
}

#[test]
fn a_session_resumes_once_codex_wrote_its_rollout_file() {
    let codex = backend(Agent::Codex).unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        codex.session_for(dir.path(), THREAD),
        SessionRun::Start(THREAD.into())
    );

    // Codex nests rollout files under a dated path inside `sessions`.
    let day = dir.path().join("sessions/2026/11/21");
    fs::create_dir_all(&day).unwrap();
    fs::write(day.join("another-thread.jsonl"), "").unwrap();
    assert_eq!(
        codex.session_for(dir.path(), THREAD),
        SessionRun::Start(THREAD.into())
    );

    fs::write(day.join(format!("{THREAD}.jsonl")), "").unwrap();
    assert_eq!(
        codex.session_for(dir.path(), THREAD),
        SessionRun::Resume(THREAD.into())
    );
}

/// A podman stand-in that records its arguments, prints the fixture at `stdout`,
/// and rewrites the mounted `auth.json` with `refreshed`, simulating Codex
/// refreshing its login mid-run.
fn fake_podman(dir: &Path, stdout: &Path, refreshed: &str) -> PathBuf {
    let args = dir.join("args");
    let auth = dir.join("codex").join("auth.json");
    support::executable(
        dir,
        "podman",
        &format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\nprintf '%s' '{refreshed}' > '{}'\nexit 0\n",
            args.display(),
            stdout.display(),
            auth.display(),
        ),
    )
}

#[tokio::test]
async fn a_run_reports_the_thread_id_usage_and_reads_the_refreshed_login_back() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("codex")).unwrap();
    fs::write(dir.path().join("codex/auth.json"), AUTH).unwrap();
    let refreshed = r#"{"tokens":{"access_token":"at-2","refresh_token":"rt-2"},"last_refresh":"2026-11-28T00:00:00Z"}"#;
    let config = WorkerConfig {
        podman: fake_podman(dir.path(), &fixture("codex-spec-run.jsonl"), refreshed),
        ..WorkerConfig::default()
    };

    let mut log = Vec::new();
    let mut reported_sessions = Vec::new();
    let report = run(
        &config,
        &codex_run(dir.path(), SessionRun::Start(THREAD.into())),
        |line| log.push(line),
        |_| {},
        |id| reported_sessions.push(id.to_owned()),
        || {},
    )
    .await;

    assert_eq!(
        report.event,
        Event::Agent(AgentResult::Spec(
            "Goal: reboot the DUT from its page.".into()
        ))
    );
    // The thread id reaches the session callback the moment `thread.started`
    // arrives, so the scheduler can store it before the run ends.
    assert_eq!(reported_sessions, [THREAD.to_owned()]);
    // Codex's final message carries no session id, so the thread id from
    // thread.started fills it in.
    assert_eq!(report.session_id.as_deref(), Some(THREAD));
    // Codex reports no dollar cost; the summed usage lets the executor estimate.
    assert_eq!(report.cost_usd, None);
    assert_eq!(
        report.token_usage,
        Some(TokenUsage {
            input_tokens: 1200,
            cached_input_tokens: 400,
            output_tokens: 350,
            reasoning_output_tokens: 120,
        })
    );
    // The login the worker refreshed is read back for the executor to keep.
    assert_eq!(report.credential.as_deref(), Some(refreshed));

    // The backend wrote its config and schema into the config directory before
    // the run.
    let config_toml = fs::read_to_string(dir.path().join("codex/config.toml")).unwrap();
    assert!(config_toml.contains("cli_auth_credentials_store = \"file\""));
    assert!(!config_toml.contains("project_doc_fallback_filenames"));
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join("codex/output-schema.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(schema, Expect::Spec.strict_schema());
}

#[test]
fn a_symlink_in_the_codex_config_dir_is_replaced_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("codex")).unwrap();
    let target = dir.path().join("outside");
    fs::write(&target, "untouched").unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("codex/config.toml")).unwrap();
    let mut run = codex_run(dir.path(), SessionRun::Start(THREAD.into()));
    run.mcp = Some(McpConfig {
        proxy: None,
        servers: vec![stdio_entry()],
    });
    backend(Agent::Codex).unwrap().prepare(&run).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    let written = dir.path().join("codex/config.toml");
    assert!(!fs::symlink_metadata(&written).unwrap().is_symlink());
    assert!(
        fs::read_to_string(&written)
            .unwrap()
            .contains("playwright-mcp")
    );
}

const LOCAL_KEY: &str = "sk-local-0123456789";

fn local_codex_run(dir: &Path, key: Option<&str>) -> WorkerRun {
    let mut run = codex_run(dir, SessionRun::Start(THREAD.into()));
    run.agent = Agent::Local;
    run.token = String::new();
    run.model = Some("unsloth/gemma-4-26B-A4B-it-GGUF:Q4_K_M".into());
    run.local = Some(LocalTarget {
        cli: Cli::Codex,
        base_url: "http://gfx1201:9931".into(),
        key: key.map(str::to_owned),
        load_timeout: Duration::from_mins(10),
    });
    run
}

#[test]
fn a_local_codex_run_points_at_the_server_with_no_subscription_login() {
    let dir = tempfile::tempdir().unwrap();
    let args = codex_args(&local_codex_run(dir.path(), None));

    let provider = args
        .iter()
        .find(|arg| arg.starts_with("model_providers.local="))
        .unwrap();
    assert!(provider.contains(r#"name = "Local""#));
    assert!(provider.contains(r#"base_url = "http://gfx1201:9931/v1""#));
    assert!(provider.contains(r#"wire_api = "responses""#));
    assert!(!provider.contains("env_key"));
    assert!(args.contains(&"model_provider=local".to_owned()));
    assert_eq!(
        value_after(&args, "--model"),
        "unsloth/gemma-4-26B-A4B-it-GGUF:Q4_K_M"
    );
    assert!(args.contains(&"model_reasoning_effort=medium".to_owned()));
    assert!(!args.contains(&"--output-schema".to_owned()));
    assert_eq!(env_names(&args), ["CODEX_HOME=/codex"]);
}

#[test]
fn a_local_codex_key_goes_by_name_only() {
    let dir = tempfile::tempdir().unwrap();
    let run = local_codex_run(dir.path(), Some(LOCAL_KEY));
    let args = codex_args(&run);

    assert!(
        args.iter()
            .any(|arg| arg.starts_with("model_providers.local=")
                && arg.contains(r#"env_key = "FACTORY_LOCAL_API_KEY""#))
    );
    assert!(env_names(&args).contains(&"FACTORY_LOCAL_API_KEY"));
    assert!(args.iter().all(|arg| !arg.contains(LOCAL_KEY)));
    assert_eq!(
        backend(Agent::Codex).unwrap().secret_env(&run),
        [("FACTORY_LOCAL_API_KEY", LOCAL_KEY)]
    );
}

#[test]
fn a_local_codex_run_writes_no_login_setting_and_no_output_schema() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("codex")).unwrap();
    backend(Agent::Codex)
        .unwrap()
        .prepare(&local_codex_run(dir.path(), None))
        .unwrap();

    let config = fs::read_to_string(dir.path().join("codex/config.toml")).unwrap();
    assert!(!config.contains("cli_auth_credentials_store"));
    assert!(!dir.path().join("codex/output-schema.json").exists());
    assert!(!dir.path().join("codex/auth.json").exists());
}

#[test]
fn only_a_local_codex_prompt_carries_the_schema() {
    let task = factory_core::Task::WriteSpec;
    let chatgpt = factory_runner::prompt(&task, Expect::Spec, Agent::Codex);
    let local = factory_runner::prompt(
        &task,
        Expect::Spec,
        factory_runner::PromptTarget::Local(Cli::Codex),
    );

    assert!(!chatgpt.contains("This agent has no structured-output mode"));
    assert!(local.contains("This agent has no structured-output mode"));
    assert!(local.contains("\"spec\""));
    let chatgpt_run = codex_args(&codex_run(
        tempfile::tempdir().unwrap().path(),
        SessionRun::Start(THREAD.into()),
    ));
    assert!(chatgpt_run.contains(&"--output-schema".to_owned()));
}

#[test]
fn a_local_codex_final_message_may_end_with_a_fenced_block() {
    let codex = backend(Agent::Codex).unwrap();
    let message = |text: &str| {
        serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":text}})
            .to_string()
    };
    let spec = AgentResult::Spec("Goal.".into());
    let bare = r#"{"outcome":"spec","questions":null,"spec":"Goal."}"#;
    let fenced = format!("Done.\n\n```json\n{bare}\n```");

    for text in [bare.to_owned(), fenced.clone()] {
        let result = codex
            .parse_local(Expect::Spec, &message(&text))
            .into_iter()
            .find_map(|item| match item {
                AgentItem::Result(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(result.event, Event::Agent(spec.clone()));
    }
    // A ChatGPT run still reads the message as bare JSON.
    let result = codex
        .parse(Expect::Spec, &message(&fenced))
        .into_iter()
        .find_map(|item| match item {
            AgentItem::Result(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(matches!(result.event, Event::AgentFailed { .. }));
}

#[tokio::test]
async fn a_local_codex_run_reads_no_auth_json_back_and_sets_the_key_in_podman_only() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("codex")).unwrap();
    let env_file = dir.path().join("env");
    let stdout = fixture("codex-spec-run.jsonl");
    let podman = support::executable(
        dir.path(),
        "podman",
        &format!(
            "#!/bin/bash\nprintf '%s' \"$FACTORY_LOCAL_API_KEY\" > '{}'\nprintf '%s' x > '{}/codex/auth.json'\ncat '{}'\n",
            env_file.display(),
            dir.path().display(),
            stdout.display(),
        ),
    );
    let config = WorkerConfig {
        podman,
        ..WorkerConfig::default()
    };
    let run_def = local_codex_run(dir.path(), Some(LOCAL_KEY));

    let mut log = Vec::new();
    let report = run(
        &config,
        &run_def,
        |line| log.push(line),
        |_| {},
        |_| {},
        || {},
    )
    .await;

    assert_eq!(fs::read_to_string(env_file).unwrap(), LOCAL_KEY);
    assert_eq!(report.credential, None);
    assert!(!format!("{log:?}").contains(LOCAL_KEY));
}
