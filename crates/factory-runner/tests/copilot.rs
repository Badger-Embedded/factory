//! The Copilot backend: its command line, its secrecy, its schema-in-prompt
//! structured output and its JSON Lines parser. The parser tests read fixtures
//! captured in the shape of `copilot -p --output-format json` at the pinned
//! `1.0.114`, plus synthetic tool lines for the events the `pong` capture did
//! not exercise; see `copilot_image.rs` for the image side and "Copilot CLI
//! facts" in docs/design.md for the event shapes.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use factory_core::{Agent, AgentResult, Event};
use factory_runner::{
    AgentItem, Expect, LogLine, McpConfig, RunResult, SessionRun, WorkerConfig, WorkerRun, backend,
    run,
};

const SESSION: &str = "d3c2f38c-6c9e-4528-9666-a9c3d7aff405";
/// A GitHub OAuth token, shaped like the one `copilot login` stores. Only its
/// bytes matter here: the backend never parses it, and the test checks it never
/// reaches the arguments.
const TOKEN: &str = "gho_0123456789abcdef0123456789abcdef0123";

fn copilot_run(dir: &Path, session: SessionRun) -> WorkerRun {
    WorkerRun {
        name: "factory-job-7-run-1".into(),
        agent: Agent::Copilot,
        workspace: dir.join("work"),
        config_dir: dir.join("copilot"),
        session,
        token: TOKEN.into(),
        expect: Expect::Spec,
        prompt: "Write the spec.".into(),
        model: None,
        effort: None,
        mcp: None,
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

fn copilot_args(run: &WorkerRun) -> Vec<String> {
    strings(&factory_runner::podman_args(&WorkerConfig::default(), run))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The `AgentItem`s the backend parses from `text`, one stage's worth.
fn parse(expect: Expect, text: &str) -> Vec<AgentItem> {
    let copilot = backend(Agent::Copilot).unwrap();
    text.lines()
        .flat_map(|line| copilot.parse(expect, line))
        .collect()
}

#[test]
fn a_new_session_runs_copilot_with_the_headless_flags_and_effort() {
    let dir = tempfile::tempdir().unwrap();
    let args = copilot_args(&copilot_run(dir.path(), SessionRun::Start(SESSION.into())));

    // The agent invocation sits after the image.
    let copilot = args.iter().position(|arg| arg == "copilot").unwrap();
    assert_eq!(args[copilot + 1], "-p");
    assert_eq!(args[copilot + 2], "Write the spec.");
    assert_eq!(value_after(&args, "--output-format"), "json");
    for flag in [
        "--allow-all",
        "--no-ask-user",
        "--no-color",
        "--no-auto-update",
        "--disable-client-telemetry",
        "--disable-builtin-mcps",
    ] {
        assert!(
            args.iter().any(|arg| arg == flag),
            "missing {flag}: {args:?}"
        );
    }
    // A new session names the factory's UUID with --session-id, not --resume.
    assert_eq!(value_after(&args, "--session-id"), SESSION);
    assert!(!args.iter().any(|arg| arg == "--resume"), "a new session");
    assert_eq!(value_after(&args, "--effort"), "medium");
    // COPILOT_HOME points Copilot at the mounted config directory.
    assert!(args.iter().any(|arg| arg == "COPILOT_HOME=/copilot"));
    assert!(
        args.iter()
            .any(|arg| *arg == format!("{}/copilot:/copilot", dir.path().display())),
        "the config directory is mounted at COPILOT_HOME: {args:?}"
    );
}

#[test]
fn a_resumed_session_names_the_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let args = copilot_args(&copilot_run(dir.path(), SessionRun::Resume(SESSION.into())));

    assert_eq!(value_after(&args, "--resume"), SESSION);
    assert!(
        !args.iter().any(|arg| arg == "--session-id"),
        "a resumed run does not also pass --session-id: {args:?}"
    );
}

#[test]
fn an_empty_model_passes_no_model_flag_and_a_set_one_is_passed() {
    let dir = tempfile::tempdir().unwrap();
    for model in [None, Some("")] {
        let mut run = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));
        run.model = model.map(str::to_owned);
        let args = copilot_args(&run);
        assert!(!args.iter().any(|arg| arg == "--model"), "{args:?}");
    }
    let mut run = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));
    run.model = Some("claude-opus-5.5".into());
    assert_eq!(
        value_after(&copilot_args(&run), "--model"),
        "claude-opus-5.5"
    );
}

#[test]
fn each_copilot_effort_is_passed_to_the_effort_flag() {
    let dir = tempfile::tempdir().unwrap();
    for effort in Agent::Copilot.efforts() {
        let mut run = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));
        run.effort = Some((*effort).to_owned());
        assert_eq!(value_after(&copilot_args(&run), "--effort"), *effort);
    }
}

#[test]
fn the_mcp_token_passes_by_name_only_and_the_login_never_enters_the_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let mut run = copilot_run(dir.path(), SessionRun::Resume(SESSION.into()));
    run.mcp = Some(McpConfig {
        proxy_url: "http://factory:8081".into(),
        token: "d34db33f-run-token".into(),
        servers: vec!["context7".into(), "docs".into()],
    });
    let args = copilot_args(&run);

    // The one MCP config holds an http entry per active server, each pointed at
    // the proxy, with the token named in the header, not spelled out.
    let config: serde_json::Value =
        serde_json::from_str(value_after(&args, "--additional-mcp-config")).unwrap();
    for server in ["context7", "docs"] {
        assert_eq!(
            config["mcpServers"][server]["url"],
            format!("http://factory:8081/mcp/{server}")
        );
        assert_eq!(config["mcpServers"][server]["type"], "http");
        assert_eq!(
            config["mcpServers"][server]["headers"]["Authorization"],
            "Bearer ${FACTORY_MCP_TOKEN}"
        );
    }
    // The run token reaches the container by name only.
    assert!(env_names(&args).contains(&"FACTORY_MCP_TOKEN"));
    assert!(
        !args.iter().any(|arg| arg.contains("d34db33f-run-token")),
        "the MCP run token must not appear in the arguments: {args:?}"
    );
    // The login passes by env name, so its bytes never appear.
    assert!(env_names(&args).contains(&"COPILOT_GITHUB_TOKEN"));
    assert!(
        !args.iter().any(|arg| arg.contains(TOKEN)),
        "the Copilot token must not appear in the arguments: {args:?}"
    );
}

#[test]
fn a_run_with_no_active_server_passes_no_mcp_config_or_token() {
    let dir = tempfile::tempdir().unwrap();
    for mcp in [None, Some(Vec::new())] {
        let mut run = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));
        run.mcp = mcp.map(|servers| McpConfig {
            proxy_url: "http://factory:8081".into(),
            token: "unused".into(),
            servers,
        });
        let args = copilot_args(&run);
        assert!(
            !args.iter().any(|arg| arg == "--additional-mcp-config"),
            "{args:?}"
        );
        assert!(!env_names(&args).contains(&"FACTORY_MCP_TOKEN"), "{args:?}");
    }
}

#[test]
fn the_reviewer_loads_the_skill_through_plugin_dir_in_both_agents_dir_configs() {
    let dir = tempfile::tempdir().unwrap();
    let run = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));

    // The image bakes the skills at /factory-agents, so the flag is passed
    // whether or not the factory mounts its own copy there.
    for config in [
        WorkerConfig::default(),
        WorkerConfig {
            agents_dir: Some(dir.path().join("agents")),
            ..WorkerConfig::default()
        },
    ] {
        let args = strings(&factory_runner::podman_args(&config, &run));
        assert_eq!(value_after(&args, "--plugin-dir"), "/factory-agents");
    }
}

#[test]
fn the_prompt_carries_the_stage_schema_so_the_agent_ends_with_its_json() {
    use factory_core::Task;
    // Copilot has no output-schema flag, so the prompt tells the agent to end
    // with the JSON and carries the stage's schema. Claude passes the schema to
    // its CLI, so its prompt does not.
    let copilot = factory_runner::prompt(&Task::WriteSpec, Expect::Spec, Agent::Copilot);
    assert!(
        copilot.contains("only the JSON"),
        "the Copilot prompt does not ask for JSON: {copilot}"
    );
    assert!(
        copilot.contains("\"outcome\""),
        "the Copilot prompt does not carry the schema"
    );
    let claude = factory_runner::prompt(&Task::WriteSpec, Expect::Spec, Agent::Claude);
    assert!(
        !claude.contains("only the JSON"),
        "Claude's prompt must not carry the schema-in-prompt wording"
    );
}

#[test]
fn the_success_fixture_parses_into_the_reply_and_a_result() {
    let items = parse(
        Expect::Spec,
        &fs::read_to_string(fixture("copilot-success.jsonl")).unwrap(),
    );

    // The streaming deltas are ephemeral and skipped, so the reply logs once. The
    // usage checkpoint and the result line add the reported counts.
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
            &LogLine::Text("pong".into()),
            &LogLine::Other(
                "Copilot usage: 1 premium request(s), 6.2503 AI credits; tokens: 4 input, 0 cache read, 24982 cache write, 4 output"
                    .into()
            ),
            &LogLine::Other("Copilot reported 1 premium request(s)".into()),
        ]
    );

    // session.start reports the id the factory resumes with.
    let sessions: Vec<&String> = items
        .iter()
        .filter_map(|item| match item {
            AgentItem::Session(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(sessions, [&SESSION.to_owned()]);

    // The reply "pong" is not the schema's JSON, so the result fails the run and
    // records no cost.
    let result = last_result(&items);
    assert_eq!(result.cost_usd, None);
    match &result.event {
        Event::AgentFailed { reason } => assert!(reason.contains("not valid JSON"), "{reason}"),
        other => panic!("expected AgentFailed, got {other:?}"),
    }
}

#[test]
fn the_auth_failure_fixture_parses_into_a_failure() {
    let items = parse(
        Expect::Spec,
        &fs::read_to_string(fixture("copilot-auth-failure.jsonl")).unwrap(),
    );
    // A result line with a non-zero exitCode and no assistant message is a
    // failure keyed off the code; the cause itself is on stderr.
    let failure = items
        .iter()
        .find_map(|item| match item {
            AgentItem::Failure(event) => Some(event),
            _ => None,
        })
        .unwrap();
    match failure {
        Event::AgentFailed { reason } => {
            assert!(reason.contains("the Copilot run failed"), "{reason}");
        }
        other => panic!("expected AgentFailed, got {other:?}"),
    }
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, AgentItem::Result(_))),
        "a failed run has no result"
    );
}

#[test]
fn a_review_result_line_fails_closed() {
    let text = fs::read_to_string(fixture("copilot-auth-failure.jsonl")).unwrap();
    let failure = parse(Expect::Review, &text)
        .into_iter()
        .find_map(|item| match item {
            AgentItem::Failure(event) => Some(event),
            _ => None,
        })
        .unwrap();
    assert!(matches!(failure, Event::ReviewFailed { .. }));
}

#[test]
fn a_valid_final_message_resolves_the_stage_result_with_or_without_a_fence() {
    for content in [
        r#"{"outcome":"spec","spec":"Goal: reboot the DUT from its page."}"#.to_owned(),
        format!(
            "```json\n{}\n```",
            r#"{"outcome":"spec","spec":"Goal: reboot the DUT from its page."}"#
        ),
    ] {
        let line = assistant_message(&content);
        let result = last_result(&parse(Expect::Spec, &line));
        assert_eq!(
            result,
            RunResult {
                event: Event::Agent(AgentResult::Spec(
                    "Goal: reboot the DUT from its page.".into()
                )),
                session_id: None,
                cost_usd: None,
            }
        );
    }
}

#[test]
fn an_invalid_final_json_message_fails_the_run() {
    let line = assistant_message("Sorry, I could not do it.");
    let result = last_result(&parse(Expect::Spec, &line));
    match result.event {
        Event::AgentFailed { reason } => assert!(reason.contains("not valid JSON"), "{reason}"),
        other => panic!("expected AgentFailed, got {other:?}"),
    }
}

#[test]
fn an_outcome_the_stage_does_not_allow_fails_the_run() {
    let line = assistant_message(r#"{"outcome":"approve"}"#);
    let result = last_result(&parse(Expect::Spec, &line));
    assert!(matches!(result.event, Event::AgentFailed { .. }));
}

#[test]
fn tool_starts_log_as_exec_edit_and_mcp_lines() {
    let shell = r#"{"type":"tool.execution_start","data":{"toolName":"bash","arguments":{"command":"rg -n reboot software"},"shellToolInfo":{"displayCommand":"rg -n reboot software","hasWriteFileRedirection":false,"possiblePaths":[]},"toolCallId":"t1"}}"#;
    let edit = r#"{"type":"tool.execution_start","data":{"toolName":"create","arguments":{"path":"src/lib.rs"},"toolCallId":"t2"}}"#;
    let mcp = r#"{"type":"tool.execution_start","data":{"toolName":"context7-get","mcpServerName":"context7","mcpToolName":"get-library-docs","arguments":{},"toolCallId":"t3"}}"#;
    let read = r#"{"type":"tool.execution_start","data":{"toolName":"view","arguments":{"path":"README.md"},"toolCallId":"t4"}}"#;

    let logs = |line: &str| -> Vec<LogLine> {
        parse(Expect::Commits, line)
            .into_iter()
            .filter_map(|item| match item {
                AgentItem::Log(line) => Some(line),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        logs(shell),
        [LogLine::Tool {
            name: "exec".into(),
            summary: "rg -n reboot software".into()
        }]
    );
    assert_eq!(
        logs(edit),
        [LogLine::Tool {
            name: "edit".into(),
            summary: "src/lib.rs".into()
        }]
    );
    assert_eq!(
        logs(mcp),
        [LogLine::Tool {
            name: "mcp__context7__get-library-docs".into(),
            summary: String::new()
        }]
    );
    // A read and other tools log nothing, the way Codex drops a read step.
    assert_eq!(logs(read), Vec::<LogLine>::new());
}

#[test]
fn a_warning_logs_and_a_failed_tool_notes_the_failure_without_failing_the_run() {
    let warning = r#"{"type":"session.warning","data":{"message":"the model fell back to auto","warningType":"model"}}"#;
    let failed = r#"{"type":"tool.execution_complete","data":{"success":false,"error":{"message":"command not found"},"toolCallId":"t1"}}"#;
    let ok = r#"{"type":"tool.execution_complete","data":{"success":true,"toolCallId":"t2"}}"#;

    let items = parse(Expect::Commits, &format!("{warning}\n{failed}\n{ok}"));
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
            &LogLine::Other("the model fell back to auto".into()),
            &LogLine::Other("tool failed: command not found".into()),
        ]
    );
    // Neither a warning nor a failed tool fails the run.
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, AgentItem::Failure(_))),
        "{items:?}"
    );
}

#[test]
fn an_ephemeral_event_gives_nothing() {
    let delta =
        r#"{"type":"assistant.message_delta","data":{"deltaContent":"pong"},"ephemeral":true}"#;
    assert_eq!(parse(Expect::Spec, delta), Vec::<AgentItem>::new());
}

#[test]
fn a_session_resumes_once_copilot_wrote_its_session_state() {
    let copilot = backend(Agent::Copilot).unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        copilot.session_for(dir.path(), SESSION),
        SessionRun::Start(SESSION.into())
    );

    // A different session's state does not resume this one.
    fs::create_dir_all(dir.path().join("session-state/another-id")).unwrap();
    assert_eq!(
        copilot.session_for(dir.path(), SESSION),
        SessionRun::Start(SESSION.into())
    );

    fs::create_dir_all(dir.path().join("session-state").join(SESSION)).unwrap();
    assert_eq!(
        copilot.session_for(dir.path(), SESSION),
        SessionRun::Resume(SESSION.into())
    );
}

/// A non-ephemeral `assistant.message` carrying `content`, the event the parser
/// reads the stage output from.
fn assistant_message(content: &str) -> String {
    serde_json::json!({
        "type": "assistant.message",
        "data": { "content": content },
    })
    .to_string()
}

/// The last `RunResult` among `items`, the one `run` keeps.
fn last_result(items: &[AgentItem]) -> RunResult {
    items
        .iter()
        .rev()
        .find_map(|item| match item {
            AgentItem::Result(result) => Some(result.clone()),
            _ => None,
        })
        .expect("a result")
}

fn intermediate_then_failure() -> String {
    let outcome = r#"{"outcome":"approve"}"#;
    let intermediate = serde_json::json!({
        "type": "assistant.message",
        "data": { "content": outcome, "toolRequests": [{"name": "bash"}] },
    });
    let result = r#"{"type":"result","exitCode":1,"usage":{"premiumRequests":0}}"#;
    format!("{intermediate}\n{result}\n")
}

#[test]
fn a_message_that_requested_a_tool_is_not_a_result() {
    let items = parse(Expect::Review, &intermediate_then_failure());
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, AgentItem::Result(_))),
        "{items:?}"
    );
    assert!(items.iter().any(|item| matches!(
        item,
        AgentItem::Log(LogLine::Text(text)) if text.contains("approve")
    )));
}

#[tokio::test]
async fn a_run_that_fails_after_an_intermediate_message_ends_on_the_failure() {
    let dir = tempfile::tempdir().unwrap();
    let stdout = dir.path().join("stdout.jsonl");
    fs::write(&stdout, intermediate_then_failure()).unwrap();
    let podman = support::executable(
        dir.path(),
        "podman",
        &format!("#!/bin/bash\ncat '{}'\nexit 1\n", stdout.display()),
    );
    let config = WorkerConfig {
        podman,
        ..WorkerConfig::default()
    };
    let mut worker = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));
    worker.expect = Expect::Review;

    let report = run(&config, &worker, |_| {}, |_| {}, |_| {}).await;

    match report.event {
        Event::ReviewFailed { reason } => {
            assert!(reason.contains("the Copilot run failed"), "{reason}");
        }
        other => panic!("expected ReviewFailed, got {other:?}"),
    }
    assert_eq!(report.cost_usd, None);
}

const COST_NOTE: &str = "Copilot runs are not counted in the job cost.";

fn cost_notes(log: &[LogLine]) -> usize {
    log.iter()
        .filter(|line| **line == LogLine::Other(COST_NOTE.into()))
        .count()
}

#[test]
fn the_failure_fixture_logs_its_premium_request_count() {
    let items = parse(
        Expect::Spec,
        &fs::read_to_string(fixture("copilot-auth-failure.jsonl")).unwrap(),
    );
    assert!(items.contains(&AgentItem::Log(LogLine::Other(
        "Copilot reported 0 premium request(s)".into()
    ))));
}

#[tokio::test]
async fn the_cost_note_is_logged_once_even_when_the_worker_prints_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let podman = support::executable(dir.path(), "podman", "#!/bin/bash\nexit 137\n");
    let config = WorkerConfig {
        podman,
        ..WorkerConfig::default()
    };
    let worker = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));

    let mut log = Vec::new();
    let report = run(&config, &worker, |line| log.push(line), |_| {}, |_| {}).await;

    assert!(matches!(report.event, Event::AgentFailed { .. }));
    assert_eq!(cost_notes(&log), 1, "{log:?}");
}

#[tokio::test]
async fn the_cost_note_is_logged_once_for_a_run_that_reaches_its_result_line() {
    let dir = tempfile::tempdir().unwrap();
    let stdout = fixture("copilot-success.jsonl");
    let podman = support::executable(
        dir.path(),
        "podman",
        &format!("#!/bin/bash\ncat '{}'\n", stdout.display()),
    );
    let config = WorkerConfig {
        podman,
        ..WorkerConfig::default()
    };
    let worker = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));

    let mut log = Vec::new();
    let report = run(&config, &worker, |line| log.push(line), |_| {}, |_| {}).await;

    assert_eq!(report.cost_usd, None);
    assert_eq!(cost_notes(&log), 1, "{log:?}");
}

#[tokio::test]
async fn a_checkpoint_is_logged_when_the_run_dies_before_its_result_line() {
    let dir = tempfile::tempdir().unwrap();
    let fixture_text = fs::read_to_string(fixture("copilot-success.jsonl")).unwrap();
    let checkpoint = fixture_text
        .lines()
        .find(|line| line.contains("session.usage_checkpoint"))
        .unwrap();
    let stdout = dir.path().join("stdout.jsonl");
    fs::write(&stdout, format!("{checkpoint}\n")).unwrap();
    let podman = support::executable(
        dir.path(),
        "podman",
        &format!("#!/bin/bash\ncat '{}'\nexit 137\n", stdout.display()),
    );
    let config = WorkerConfig {
        podman,
        ..WorkerConfig::default()
    };
    let worker = copilot_run(dir.path(), SessionRun::Start(SESSION.into()));

    let mut log = Vec::new();
    let report = run(&config, &worker, |line| log.push(line), |_| {}, |_| {}).await;

    assert!(matches!(report.event, Event::AgentFailed { .. }));
    assert!(
        log.contains(&LogLine::Other(
            "Copilot usage: 1 premium request(s), 6.2503 AI credits; tokens: 4 input, 0 cache read, 24982 cache write, 4 output"
                .into()
        )),
        "{log:?}"
    );
}
