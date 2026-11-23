mod support;

use std::fs;
use std::path::{Path, PathBuf};

use factory_core::{
    Agent, AgentResult, Command, DiffVerdict, Event, Gates, Job, PlanUsage, PlannedPr,
    PrDescription, Question, Stage, Task, UsageWindow,
};
use factory_runner::{
    Expect, LogLine, McpConfig, McpServerStatus, PlacedFile, ResultMessage, SessionRun, StreamItem,
    WorkerConfig, WorkerRun, backend, event_from_result, job_prompt, parse_line, place_files,
    place_images, podman_args, prompt, remove_container, run, show_files, show_images,
};
use serde_json::{Value, json};

fn ready() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.".into(),
    }
}

const SESSION: &str = "0b6f5c1e-8d1a-4c3e-9a57-3c1f2a9d7e10";

fn result(structured_output: Option<Value>) -> ResultMessage {
    ResultMessage {
        subtype: "success".into(),
        is_error: false,
        session_id: Some(SESSION.into()),
        result: Some("done".into()),
        structured_output,
        total_cost_usd: Some(0.1),
        errors: Vec::new(),
    }
}

fn failure_reason(event: Event) -> String {
    match event {
        Event::AgentFailed { reason } => reason,
        other => panic!("expected AgentFailed, got {other:?}"),
    }
}

fn worker_run(dir: &Path, session: SessionRun) -> WorkerRun {
    WorkerRun {
        name: "factory-job-7-run-3".into(),
        agent: Agent::Claude,
        workspace: dir.join("work"),
        config_dir: dir.join("claude"),
        session,
        token: "sk-ant-oat01-token".into(),
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

/// A podman stand-in that records its arguments and the Claude token from its
/// environment, prints `stdout`, writes `stderr` and exits with `code`.
fn fake_podman(dir: &Path, stdout: &Path, stderr: &str, code: i32) -> PathBuf {
    let args = dir.join("args");
    let token = dir.join("token");
    support::executable(
        dir,
        "podman",
        &format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' \"$CLAUDE_CODE_OAUTH_TOKEN\" > '{}'\ncat '{}'\nprintf '%s' '{stderr}' >&2\nexit {code}\n",
            args.display(),
            token.display(),
            stdout.display()
        ),
    )
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn assistant_text_and_tool_calls_become_log_lines() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test\necho done"}},{"type":"thinking","thinking":"hidden"}]}}"#;
    assert_eq!(
        parse_line(line),
        vec![
            StreamItem::Log(LogLine::Text("Reading.".into())),
            StreamItem::Log(LogLine::Tool {
                name: "Bash".into(),
                summary: "cargo test".into()
            }),
        ]
    );
}

#[test]
fn other_messages_give_no_log_lines_and_plain_text_is_kept() {
    assert_eq!(
        parse_line(r#"{"type":"system","subtype":"init"}"#),
        vec![StreamItem::Init(vec![])]
    );
    assert_eq!(
        parse_line(r#"{"type":"system","subtype":"compact_boundary"}"#),
        vec![]
    );
    assert_eq!(
        parse_line(r#"{"type":"user","message":{"content":[]}}"#),
        vec![]
    );
    assert_eq!(parse_line("  "), vec![]);
    assert_eq!(
        parse_line("Trying to pull localhost/factory-worker:latest..."),
        vec![StreamItem::Log(LogLine::Other(
            "Trying to pull localhost/factory-worker:latest...".into()
        ))]
    );
}

#[test]
fn the_init_line_reports_each_mcp_server_status() {
    let line = r#"{"type":"system","subtype":"init","session_id":"s","mcp_servers":[{"name":"context7","status":"connected"},{"name":"acme","status":"failed"}]}"#;
    assert_eq!(
        parse_line(line),
        vec![StreamItem::Init(vec![
            McpServerStatus {
                name: "context7".into(),
                status: "connected".into(),
            },
            McpServerStatus {
                name: "acme".into(),
                status: "failed".into(),
            },
        ])]
    );
}

/// A `rate_limit_event` line as Claude Code 2.1.330 writes it.
const RATE_LIMIT_EVENT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1795234800,"rateLimitType":"five_hour","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.77,"resetsAt":1795234800,"status":"allowed"},"seven_day":{"utilization":0.33,"resetsAt":1795676400,"status":"allowed"}}},"uuid":"u","session_id":"s"}"#;

fn reported_usage() -> PlanUsage {
    PlanUsage {
        five_hour: Some(UsageWindow {
            utilization: 0.77,
            resets_at: 1_795_234_800,
        }),
        seven_day: Some(UsageWindow {
            utilization: 0.33,
            resets_at: 1_795_676_400,
        }),
        fable: None,
    }
}

#[test]
fn the_rate_limit_event_reports_plan_usage() {
    assert_eq!(
        parse_line(RATE_LIMIT_EVENT),
        vec![StreamItem::PlanUsage(reported_usage())]
    );
    // A rate limit event with no window the tile shows gives nothing.
    assert_eq!(
        parse_line(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#),
        vec![]
    );
}

#[tokio::test]
async fn a_run_passes_each_plan_usage_report_to_its_callback() {
    let dir = tempfile::tempdir().unwrap();
    let stdout = dir.path().join("stdout");
    fs::write(&stdout, format!("{RATE_LIMIT_EVENT}\n")).unwrap();
    let config = WorkerConfig {
        podman: fake_podman(dir.path(), &stdout, "", 0),
        ..WorkerConfig::default()
    };
    let worker = worker_run(dir.path(), SessionRun::Start(SESSION.into()));

    let mut reports = Vec::new();
    run(
        &config,
        &worker,
        |_| {},
        |usage| reports.push(usage),
        |_| {},
    )
    .await;

    assert_eq!(reports, [reported_usage()]);
}

#[test]
fn the_result_line_is_parsed() {
    let items = parse_line(
        r#"{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"s","errors":["Reached maximum number of turns (40)"]}"#,
    );
    let [StreamItem::Result(result)] = items.as_slice() else {
        panic!("expected one result, got {items:?}");
    };
    assert_eq!(result.subtype, "error_max_turns");
    assert!(result.is_error);
    assert_eq!(result.errors, ["Reached maximum number of turns (40)"]);
}

#[test]
fn each_agent_stage_expects_its_own_outcome() {
    assert_eq!(Expect::for_stage(&Stage::Clarifying), Some(Expect::Clarify));
    assert_eq!(Expect::for_stage(&Stage::Spec), Some(Expect::Spec));
    assert_eq!(Expect::for_stage(&Stage::Plan), Some(Expect::Plan));
    for stage in [
        Stage::Implementing { pr: 0 },
        Stage::Verifying { pr: 0 },
        Stage::CiAndReview { pr: 1 },
    ] {
        assert_eq!(Expect::for_stage(&stage), Some(Expect::Commits));
    }
    assert_eq!(Expect::for_stage(&Stage::PrOpen { pr: 0 }), None);
    assert_eq!(Expect::for_stage(&Stage::Requested), None);
}

#[test]
fn the_schema_allows_questions_and_the_stage_outcome() {
    let schema = Expect::Plan.schema();
    assert_eq!(
        schema["properties"]["outcome"]["enum"],
        json!(["questions", "plan"])
    );
    assert_eq!(schema["required"], json!(["outcome"]));
    assert!(schema["properties"]["plan"].is_object());
    assert!(Expect::Commits.schema()["properties"].get("plan").is_none());
    assert_eq!(
        Expect::Commits.schema()["properties"]["pull_request"]["required"],
        json!(["title", "body"])
    );
}

#[test]
fn structured_outputs_become_agent_results() {
    let cases = [
        (
            Expect::Clarify,
            json!({"outcome": "questions", "questions": ["Which board?"]}),
            AgentResult::Questions(vec![Question {
                text: "Which board?".into(),
            }]),
        ),
        (
            Expect::Clarify,
            json!({"outcome": "clarified"}),
            AgentResult::Clarified,
        ),
        (
            Expect::Spec,
            json!({"outcome": "spec", "spec": "Goal: x"}),
            AgentResult::Spec("Goal: x".into()),
        ),
        (
            Expect::Plan,
            json!({"outcome": "plan", "plan": [{"title": "a: b", "summary": "c"}]}),
            AgentResult::Plan(vec![PlannedPr {
                title: "a: b".into(),
                summary: "c".into(),
                number: None,
                description: None,
                notes: None,
            }]),
        ),
        (
            Expect::Plan,
            json!({"outcome": "plan", "plan": [
                {"title": "a: b", "summary": "c", "notes": "Change store.rs add_cost."},
                {"title": "d: e", "summary": "f", "notes": "  "}
            ]}),
            AgentResult::Plan(vec![
                PlannedPr {
                    title: "a: b".into(),
                    summary: "c".into(),
                    number: None,
                    description: None,
                    notes: Some("Change store.rs add_cost.".into()),
                },
                PlannedPr {
                    title: "d: e".into(),
                    summary: "f".into(),
                    number: None,
                    description: None,
                    notes: None,
                },
            ]),
        ),
        (
            Expect::Commits,
            json!({"outcome": "commits_ready", "pull_request": {
                "title": "maestrod: add a reboot endpoint",
                "body": "## Why\n\nThe device page needs it."
            }}),
            AgentResult::CommitsReady(ready()),
        ),
    ];
    for (expect, output, want) in cases {
        assert_eq!(
            event_from_result(expect, &result(Some(output))),
            Event::Agent(want)
        );
    }
}

#[test]
fn an_outcome_for_another_stage_fails_the_run() {
    let reason = failure_reason(event_from_result(
        Expect::Plan,
        &result(Some(json!({"outcome": "spec", "spec": "x"}))),
    ));
    assert!(reason.contains("\"spec\""), "{reason}");
}

#[test]
fn incomplete_outputs_fail_the_run() {
    for (expect, output) in [
        (Expect::Clarify, json!({"outcome": "questions"})),
        (Expect::Spec, json!({"outcome": "spec"})),
        (Expect::Spec, json!({"outcome": "spec", "spec": "  "})),
        (Expect::Spec, json!({"spec": "no outcome"})),
        (Expect::Commits, json!({"outcome": "commits_ready"})),
        (
            Expect::Commits,
            json!({"outcome": "commits_ready", "pull_request": {"title": "x", "body": " "}}),
        ),
    ] {
        let event = event_from_result(expect, &result(Some(output.clone())));
        assert!(
            matches!(event, Event::AgentFailed { .. }),
            "{output} gave {event:?}"
        );
    }
    let reason = failure_reason(event_from_result(Expect::Spec, &result(None)));
    assert!(reason.contains("without structured output"), "{reason}");
}

#[test]
fn an_error_result_fails_the_run_with_its_reason() {
    let mut message = result(None);
    message.subtype = "error_max_turns".into();
    message.is_error = true;
    message.result = None;
    message.errors = vec!["Reached maximum number of turns (40)".into()];

    let reason = failure_reason(event_from_result(Expect::Commits, &message));
    assert!(reason.contains("error_max_turns"), "{reason}");
    assert!(reason.contains("maximum number of turns"), "{reason}");
}

#[test]
fn the_answer_prompt_pairs_questions_with_answers() {
    let text = prompt(
        &Task::Answer {
            questions: vec![
                Question {
                    text: "Which board?".into(),
                },
                Question {
                    text: "Confirm first?".into(),
                },
            ],
            answers: vec!["rpi5".into(), "yes".into()],
        },
        Expect::Clarify,
        Agent::Claude,
    );
    assert!(text.contains("Q: Which board?\nA: rpi5"));
    assert!(text.contains("Q: Confirm first?\nA: yes"));
    assert!(text.contains("\"clarified\""));
}

#[test]
fn every_prompt_names_the_outcome_that_ends_the_task() {
    let pr = PlannedPr {
        title: "maestrod: add a reboot endpoint".into(),
        summary: "POST /reboot".into(),
        number: None,
        description: None,
        notes: None,
    };
    let text = prompt(&Task::Implement { pr }, Expect::Commits, Agent::Claude);
    assert!(text.contains("maestrod: add a reboot endpoint"));
    assert!(text.contains("\"commits_ready\""));
    assert!(text.contains("Do not push"));
}

#[test]
fn the_plan_schema_asks_for_notes_on_each_pull_request() {
    let schema = Expect::Plan.schema();
    let item = &schema["properties"]["plan"]["items"];
    assert_eq!(item["required"], json!(["title", "summary", "notes"]));
    assert!(item["properties"]["notes"].is_object());
    let text = prompt(&Task::WritePlan, Expect::Plan, Agent::Claude);
    assert!(text.contains("notes"));
}

#[test]
fn the_implement_prompt_carries_the_planning_notes() {
    let mut pr = PlannedPr {
        title: "maestrod: add a reboot endpoint".into(),
        summary: "POST /reboot".into(),
        number: None,
        description: None,
        notes: None,
    };
    let without = prompt(
        &Task::Implement { pr: pr.clone() },
        Expect::Commits,
        Agent::Claude,
    );
    assert!(!without.contains("planning run left"));

    pr.notes = Some("Add the route in maestrod/src/api.rs next to power_off.".into());
    let with = prompt(&Task::Implement { pr }, Expect::Commits, Agent::Claude);
    assert!(with.contains("planning run left these notes"));
    assert!(with.contains("Add the route in maestrod/src/api.rs next to power_off."));
}

#[test]
fn podman_args_give_the_worker_its_token_limits_and_mounts() {
    let config = WorkerConfig {
        agents_dir: Some(PathBuf::from("/srv/factory/agents")),
        volumes: vec!["factory-nix:/nix".into()],
        ..WorkerConfig::default()
    };
    let args = strings(&podman_args(
        &config,
        &worker_run(Path::new("/jobs/7"), SessionRun::Start(SESSION.into())),
    ));

    assert_eq!(args[0], "run");
    assert_eq!(value_after(&args, "--name"), "factory-job-7-run-3");
    assert_eq!(value_after(&args, "--memory"), "3g");
    assert_eq!(value_after(&args, "--cpus"), "4");
    // The token passes through podman's environment, named by an `--env` with no
    // value. It is never a `--secret` and never appears in the arguments.
    assert_eq!(value_after(&args, "--env"), "CLAUDE_CODE_OAUTH_TOKEN");
    assert!(!args.iter().any(|arg| arg == "--secret"));
    assert!(
        !args.iter().any(|arg| arg.contains("sk-ant-oat01-token")),
        "the token must not appear in the arguments"
    );
    for volume in [
        "/jobs/7/work:/work",
        "/jobs/7/claude:/claude",
        "/srv/factory/agents:/factory-agents:ro",
        "factory-nix:/nix",
    ] {
        assert!(args.iter().any(|arg| arg == volume), "missing {volume}");
    }
    assert_eq!(value_after(&args, "--session-id"), SESSION);
    assert!(!args.iter().any(|arg| arg == "--resume"));
    assert_eq!(value_after(&args, "--plugin-dir"), "/factory-agents");
    assert_eq!(value_after(&args, "--output-format"), "stream-json");
    let settings: Value = serde_json::from_str(value_after(&args, "--settings")).unwrap();
    assert_eq!(
        settings["attribution"],
        json!({"commit": "", "pr": "", "sessionUrl": false})
    );
    assert_eq!(value_after(&args, "-p"), "Write the spec.");
    let schema: Value = serde_json::from_str(value_after(&args, "--json-schema")).unwrap();
    assert_eq!(schema, Expect::Spec.schema());
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("GH_TOKEN") || arg.contains("GITHUB_TOKEN")),
        "workers get no GitHub credentials"
    );
}

#[test]
fn a_later_run_resumes_the_session() {
    let args = strings(&podman_args(
        &WorkerConfig::default(),
        &worker_run(Path::new("/jobs/7"), SessionRun::Resume(SESSION.into())),
    ));
    assert_eq!(value_after(&args, "--resume"), SESSION);
    assert!(!args.iter().any(|arg| arg == "--session-id"));
    assert!(!args.iter().any(|arg| arg == "--plugin-dir"));
}

#[tokio::test]
async fn a_finished_run_reports_its_event_session_cost_and_log() {
    let dir = tempfile::tempdir().unwrap();
    let config = WorkerConfig {
        podman: fake_podman(dir.path(), &fixture("spec-run.jsonl"), "", 0),
        ..WorkerConfig::default()
    };
    let mut log = Vec::new();

    let report = run(
        &config,
        &worker_run(dir.path(), SessionRun::Start(SESSION.into())),
        |line| log.push(line),
        |_| {},
        |_| {},
    )
    .await;

    assert_eq!(
        report.event,
        Event::Agent(AgentResult::Spec(
            "Goal: reboot the DUT from its page.".into()
        ))
    );
    assert_eq!(report.session_id.as_deref(), Some(SESSION));
    assert_eq!(report.cost_usd, Some(0.4213));
    assert_eq!(
        log,
        [
            LogLine::Text("I'll read the device page first.".into()),
            LogLine::Tool {
                name: "Read".into(),
                summary: "/work/software/web/device.rs".into()
            },
            LogLine::Tool {
                name: "Bash".into(),
                summary: "rg -n reboot software".into()
            },
        ]
    );
    let recorded = fs::read_to_string(dir.path().join("args")).unwrap();
    assert!(recorded.starts_with("run\n"));
}

#[tokio::test]
async fn the_token_reaches_the_worker_environment_not_its_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let config = WorkerConfig {
        podman: fake_podman(dir.path(), &fixture("spec-run.jsonl"), "", 0),
        ..WorkerConfig::default()
    };
    let mut worker = worker_run(dir.path(), SessionRun::Start(SESSION.into()));
    worker.token = "sk-ant-oat01-secret".into();

    run(&config, &worker, |_| {}, |_| {}, |_| {}).await;

    // podman saw the token in its environment, so it passes it to the container,
    // but the token is in none of the arguments.
    assert_eq!(
        fs::read_to_string(dir.path().join("token")).unwrap(),
        "sk-ant-oat01-secret"
    );
    let args = fs::read_to_string(dir.path().join("args")).unwrap();
    assert!(
        !args.contains("sk-ant-oat01-secret"),
        "the token must not appear in the arguments: {args}"
    );
    assert!(args.contains("CLAUDE_CODE_OAUTH_TOKEN"));
}

#[tokio::test]
async fn a_run_reports_each_mcp_server_the_init_line_shows_as_not_connected() {
    let dir = tempfile::tempdir().unwrap();
    let config = WorkerConfig {
        podman: fake_podman(dir.path(), &fixture("mcp-unavailable-run.jsonl"), "", 0),
        ..WorkerConfig::default()
    };

    let report = run(
        &config,
        &worker_run(dir.path(), SessionRun::Start(SESSION.into())),
        |_| {},
        |_| {},
        |_| {},
    )
    .await;

    // The connected server causes no warning; the failed one does, with a
    // reason and no credential.
    assert_eq!(report.mcp_unavailable.len(), 1);
    assert_eq!(report.mcp_unavailable[0].server, "acme");
    assert!(
        report.mcp_unavailable[0].reason.contains("failed"),
        "{}",
        report.mcp_unavailable[0].reason
    );
}

#[tokio::test]
async fn a_worker_that_dies_without_a_result_fails_with_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    fs::write(&empty, "").unwrap();
    let config = WorkerConfig {
        podman: fake_podman(
            dir.path(),
            &empty,
            "Error: short-name resolution enforced but cannot prompt without a TTY",
            125,
        ),
        ..WorkerConfig::default()
    };

    let report = run(
        &config,
        &worker_run(dir.path(), SessionRun::Start(SESSION.into())),
        |_| {},
        |_| {},
        |_| {},
    )
    .await;

    let reason = failure_reason(report.event);
    assert!(reason.contains("125"), "{reason}");
    assert!(reason.contains("short-name resolution"), "{reason}");
    assert_eq!(report.session_id, None);
}

#[tokio::test]
async fn a_missing_podman_binary_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let config = WorkerConfig {
        podman: dir.path().join("no-such-podman"),
        ..WorkerConfig::default()
    };
    let report = run(
        &config,
        &worker_run(dir.path(), SessionRun::Start(SESSION.into())),
        |_| {},
        |_| {},
        |_| {},
    )
    .await;
    assert!(failure_reason(report.event).contains("could not start"));
}

#[test]
fn a_session_resumes_once_claude_code_wrote_its_transcript() {
    let claude = backend(Agent::Claude).unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        claude.session_for(dir.path(), SESSION),
        SessionRun::Start(SESSION.into())
    );

    let project = dir.path().join("projects/-work");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("another-session.jsonl"), "").unwrap();
    assert_eq!(
        claude.session_for(dir.path(), SESSION),
        SessionRun::Start(SESSION.into())
    );

    fs::write(project.join(format!("{SESSION}.jsonl")), "").unwrap();
    assert_eq!(
        claude.session_for(dir.path(), SESSION),
        SessionRun::Resume(SESSION.into())
    );
}

#[tokio::test]
async fn remove_container_runs_podman_rm_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    fs::write(&empty, "").unwrap();
    let ok = WorkerConfig {
        podman: fake_podman(dir.path(), &empty, "", 0),
        ..WorkerConfig::default()
    };
    remove_container(&ok, "factory-job-7").await.unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("args")).unwrap(),
        "rm\n--force\n--ignore\nfactory-job-7\n"
    );

    let failing = WorkerConfig {
        podman: fake_podman(dir.path(), &empty, "Error: cannot connect", 125),
        ..WorkerConfig::default()
    };
    let err = remove_container(&failing, "factory-job-7")
        .await
        .unwrap_err();
    assert!(err.contains("cannot connect"), "{err}");
}

#[test]
fn the_rebase_prompt_names_the_fetched_base() {
    let text = prompt(
        &Task::Rebase {
            base: "main".into(),
        },
        Expect::Commits,
        Agent::Claude,
    );
    assert!(text.contains("origin/main"));
    assert!(text.contains("Do not push"));
    assert!(text.contains("\"commits_ready\""));
}

fn init_repo(dir: &Path) {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("README.md"), "hello").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
}

#[test]
fn placed_images_keep_their_bytes_and_leave_git_status_clean() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    init_repo(&work);
    let uploads = dir.path().join("images");
    std::fs::create_dir(&uploads).unwrap();
    let png = [0x89, b'P', b'N', b'G', 0, 1, 2, 255];
    let gif = *b"GIF89a\x00\xff";
    std::fs::write(uploads.join("image-1.png"), png).unwrap();
    std::fs::write(uploads.join("image-2.gif"), gif).unwrap();

    let paths = place_images(
        &work,
        &[uploads.join("image-1.png"), uploads.join("image-2.gif")],
    )
    .unwrap();

    assert_eq!(
        paths,
        [
            "/work/.git/factory-images/image-1.png",
            "/work/.git/factory-images/image-2.gif"
        ]
    );
    let host = work.join(".git/factory-images");
    assert_eq!(std::fs::read(host.join("image-1.png")).unwrap(), png);
    assert_eq!(std::fs::read(host.join("image-2.gif")).unwrap(), gif);
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(status.status.success());
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
}

#[test]
fn the_clarify_prompt_lists_the_images_and_asks_the_agent_to_look() {
    let base = prompt(
        &Task::Clarify {
            request: "Add a reboot button".into(),
        },
        Expect::Clarify,
        Agent::Claude,
    );
    let paths = vec![
        "/work/.git/factory-images/image-1.png".to_owned(),
        "/work/.git/factory-images/image-2.gif".to_owned(),
    ];
    let text = show_images(&base, &paths);
    assert!(text.starts_with(&base));
    assert!(text.contains("Look at each one"));
    assert!(text.contains("- /work/.git/factory-images/image-1.png\n"));
    assert!(text.contains("- /work/.git/factory-images/image-2.gif\n"));
}

#[test]
fn a_prompt_without_images_stays_the_same() {
    assert_eq!(show_images("Write the spec.", &[]), "Write the spec.");
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(place_images(dir.path(), &[]).unwrap(), Vec::<String>::new());
}

#[test]
fn placed_files_keep_their_bytes_and_leave_git_status_clean() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    init_repo(&work);
    let uploads = dir.path().join("files");
    std::fs::create_dir(&uploads).unwrap();
    let log = b"a log line\n";
    let zip = b"PK\x03\x04 bytes";
    std::fs::write(uploads.join("file-1.txt"), log).unwrap();
    std::fs::write(uploads.join("file-2.zip"), zip).unwrap();

    let placed = place_files(
        &work,
        &[
            (uploads.join("file-1.txt"), "server.log".into()),
            (uploads.join("file-2.zip"), "bundle.zip".into()),
        ],
    )
    .unwrap();

    assert_eq!(
        placed,
        [
            PlacedFile {
                path: "/work/.git/factory-files/file-1.txt".into(),
                original_name: "server.log".into(),
                is_zip: false,
            },
            PlacedFile {
                path: "/work/.git/factory-files/file-2.zip".into(),
                original_name: "bundle.zip".into(),
                is_zip: true,
            },
        ]
    );
    let host = work.join(".git/factory-files");
    assert_eq!(std::fs::read(host.join("file-1.txt")).unwrap(), log);
    assert_eq!(std::fs::read(host.join("file-2.zip")).unwrap(), zip);
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(status.status.success());
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
}

#[test]
fn the_clarify_prompt_lists_the_files_with_their_names_and_unzip_steps() {
    let base = prompt(
        &Task::Clarify {
            request: "Add a reboot button".into(),
        },
        Expect::Clarify,
        Agent::Claude,
    );
    let files = vec![
        PlacedFile {
            path: "/work/.git/factory-files/file-1.txt".into(),
            original_name: "server.log".into(),
            is_zip: false,
        },
        PlacedFile {
            path: "/work/.git/factory-files/file-2.zip".into(),
            original_name: "bundle.zip".into(),
            is_zip: true,
        },
    ];
    let text = show_files(&base, &files);
    assert!(text.starts_with(&base));
    assert!(text.contains("- /work/.git/factory-files/file-1.txt (server.log)\n"));
    assert!(text.contains("- /work/.git/factory-files/file-2.zip (bundle.zip)\n"));
    assert!(text.contains("unzip -l"));
    assert!(text.contains("unzip -d"));
}

#[test]
fn a_prompt_without_files_stays_the_same() {
    assert_eq!(show_files("Write the spec.", &[]), "Write the spec.");
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        place_files(dir.path(), &[]).unwrap(),
        Vec::<PlacedFile>::new()
    );
}

fn args_with_model(model: Option<&str>) -> Vec<String> {
    let mut run = worker_run(Path::new("/jobs/7"), SessionRun::Start(SESSION.into()));
    run.model = model.map(str::to_owned);
    strings(&podman_args(&WorkerConfig::default(), &run))
}

#[test]
fn podman_args_pass_no_model_flag_when_the_model_is_empty() {
    for model in [None, Some("")] {
        let args = args_with_model(model);
        assert!(!args.iter().any(|arg| arg == "--model"), "{args:?}");
    }
}

#[test]
fn podman_args_pass_the_model_to_claude_for_each_stage() {
    for (expect, model) in [
        (Expect::Clarify, "haiku"),
        (Expect::Spec, "opus"),
        (Expect::Plan, "sonnet"),
        (Expect::Commits, "claude-sonnet-5-5"),
    ] {
        let mut run = worker_run(Path::new("/jobs/7"), SessionRun::Resume(SESSION.into()));
        run.expect = expect;
        run.model = Some(model.into());
        let args = strings(&podman_args(&WorkerConfig::default(), &run));
        assert_eq!(value_after(&args, "--model"), model);
        let image = args.iter().position(|arg| arg == "claude").unwrap();
        assert!(args.iter().position(|arg| arg == "--model").unwrap() > image);
    }
}

fn args_with_effort(expect: Expect, effort: Option<&str>) -> Vec<String> {
    let mut run = worker_run(Path::new("/jobs/7"), SessionRun::Resume(SESSION.into()));
    run.expect = expect;
    run.effort = effort.map(str::to_owned);
    strings(&podman_args(&WorkerConfig::default(), &run))
}

#[test]
fn podman_args_default_to_medium_effort_for_each_stage() {
    for expect in [
        Expect::Clarify,
        Expect::Spec,
        Expect::Plan,
        Expect::Commits,
        Expect::FixCi,
        Expect::Review,
    ] {
        let args = args_with_effort(expect, None);
        assert_eq!(value_after(&args, "--effort"), "medium", "{expect:?}");
        let image = args.iter().position(|arg| arg == "claude").unwrap();
        assert!(args.iter().position(|arg| arg == "--effort").unwrap() > image);
    }
}

#[test]
fn podman_args_pass_the_set_effort_to_claude() {
    for effort in Agent::Claude.efforts() {
        let args = args_with_effort(Expect::Commits, Some(effort));
        assert_eq!(value_after(&args, "--effort"), *effort, "{effort}");
    }
}

#[test]
fn podman_args_pass_no_mcp_config_without_an_active_server() {
    // A run with no MCP, and a config whose server list is empty, both add
    // nothing, so the arguments match a run from before MCP.
    for mcp in [None, Some(Vec::new())] {
        let mut run = worker_run(Path::new("/jobs/7"), SessionRun::Start(SESSION.into()));
        run.mcp = mcp.map(|servers| McpConfig {
            proxy_url: "http://factory:8081".into(),
            token: "unused".into(),
            servers,
        });
        let args = strings(&podman_args(&WorkerConfig::default(), &run));
        assert!(!args.iter().any(|arg| arg == "--mcp-config"), "{args:?}");
    }
}

#[test]
fn podman_args_give_each_active_server_a_proxy_entry_with_the_run_token() {
    let mut run = worker_run(Path::new("/jobs/7"), SessionRun::Resume(SESSION.into()));
    run.mcp = Some(McpConfig {
        proxy_url: "http://factory:8081".into(),
        token: "d34db33f".into(),
        servers: vec!["context7".into(), "docs".into()],
    });
    let args = strings(&podman_args(&WorkerConfig::default(), &run));

    // The flag sits in the claude arguments, after the image.
    let image = args.iter().position(|arg| arg == "claude").unwrap();
    assert!(args.iter().position(|arg| arg == "--mcp-config").unwrap() > image);

    let config: Value = serde_json::from_str(value_after(&args, "--mcp-config")).unwrap();
    let servers = config["mcpServers"].as_object().unwrap();
    assert_eq!(servers.len(), 2);
    for name in ["context7", "docs"] {
        let entry = &servers[name];
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], format!("http://factory:8081/mcp/{name}"));
        assert_eq!(
            entry["headers"],
            json!({ "Authorization": "Bearer d34db33f" }),
            "the run token is the entry's only header"
        );
    }
}

#[test]
fn podman_args_attach_the_worker_to_the_configured_network() {
    let config = WorkerConfig {
        network: Some("factory".into()),
        ..WorkerConfig::default()
    };
    let run = worker_run(Path::new("/jobs/7"), SessionRun::Start(SESSION.into()));
    let args = strings(&podman_args(&config, &run));

    assert_eq!(value_after(&args, "--network"), "factory");
    // The network flag is a `podman run` flag, before the image.
    let image = args.iter().position(|arg| arg == "claude").unwrap();
    assert!(args.iter().position(|arg| arg == "--network").unwrap() < image);

    // The default leaves podman's own network, so no run before MCP changes.
    let plain = strings(&podman_args(&WorkerConfig::default(), &run));
    assert!(!plain.iter().any(|arg| arg == "--network"), "{plain:?}");
}

fn planned(title: &str) -> PlannedPr {
    PlannedPr {
        title: title.into(),
        summary: format!("{title} summary"),
        number: None,
        description: None,
        notes: None,
    }
}

/// A job after the plan is approved, with one answered question.
fn approved_job(job: Job) -> Job {
    let mut job = job;
    let step = |job: &mut Job, event| job.apply(event).unwrap();
    step(&mut job, Event::Start);
    step(
        &mut job,
        Event::Agent(AgentResult::Questions(vec![Question {
            text: "Which board?".into(),
        }])),
    );
    step(&mut job, Event::Answer(vec!["rpi5".into()]));
    step(&mut job, Event::Agent(AgentResult::Clarified));
    step(
        &mut job,
        Event::Agent(AgentResult::Spec("Reboot over HTTP.".into())),
    );
    step(&mut job, Event::Approve);
    step(
        &mut job,
        Event::Agent(AgentResult::Plan(vec![
            planned("maestrod: add a reboot endpoint"),
            planned("maestro-cli: add a reboot command"),
        ])),
    );
    job
}

fn run_prompt(job: &Job, expect: Expect) -> String {
    match job.next_command() {
        Some(Command::RunAgent { task, .. }) => job_prompt(job, &task, expect, Agent::Claude, &[]),
        other => panic!("no agent run: {other:?}"),
    }
}

#[test]
fn the_spec_prompt_holds_the_request_and_the_answers() {
    let mut job = Job::new("Add a reboot button", Gates::default());
    job.apply(Event::Start).unwrap();
    job.apply(Event::Agent(AgentResult::Questions(vec![Question {
        text: "Which board?".into(),
    }])))
    .unwrap();
    job.apply(Event::Answer(vec!["rpi5".into()])).unwrap();
    job.apply(Event::Agent(AgentResult::Clarified)).unwrap();
    let text = run_prompt(&job, Expect::Spec);
    assert!(text.contains("Add a reboot button"));
    assert!(text.contains("Q: Which board?\nA: rpi5"));
    assert!(text.contains("Write the spec"));
}

#[test]
fn the_plan_prompt_holds_the_spec_text() {
    let mut job = Job::new("Add a reboot button", Gates::default());
    job.apply(Event::Start).unwrap();
    job.apply(Event::Agent(AgentResult::Clarified)).unwrap();
    job.apply(Event::Agent(AgentResult::Spec("Reboot over HTTP.".into())))
        .unwrap();
    job.apply(Event::Approve).unwrap();
    let text = run_prompt(&job, Expect::Plan);
    assert!(text.contains("Add a reboot button"));
    assert!(text.contains("Reboot over HTTP."));
    assert!(text.contains("Split the work into pull requests"));
}

#[test]
fn the_implement_prompt_holds_the_spec_and_its_planned_pull_request() {
    let mut job = approved_job(Job::new("Add a reboot button", Gates::default()));
    job.apply(Event::Approve).unwrap();
    job.apply(Event::Agent(AgentResult::CommitsReady(ready())))
        .unwrap();
    let first = run_prompt_for_pr(&job, 0);
    assert!(first.contains("Add a reboot button"));
    assert!(first.contains("Q: Which board?\nA: rpi5"));
    assert!(first.contains("Reboot over HTTP."));
    assert!(first.contains("maestrod: add a reboot endpoint summary"));
    assert!(first.contains("2. maestro-cli: add a reboot command"));
    assert!(!first.contains("maestro-cli: add a reboot command summary"));
}

fn run_prompt_for_pr(job: &Job, pr: usize) -> String {
    let task = Task::Implement {
        pr: job.plan()[pr].clone(),
    };
    job_prompt(job, &task, Expect::Commits, Agent::Claude, &[])
}

#[test]
fn a_job_from_before_fresh_sessions_gets_the_short_prompts() {
    let job = approved_job(Job::new("Add a reboot button", Gates::default()));
    let mut stored = serde_json::to_value(&job).unwrap();
    stored
        .as_object_mut()
        .unwrap()
        .remove("fresh_sessions")
        .unwrap();
    let old: Job = serde_json::from_value(stored).unwrap();
    let text = job_prompt(&old, &Task::WritePlan, Expect::Plan, Agent::Claude, &[]);
    assert_eq!(text, prompt(&Task::WritePlan, Expect::Plan, Agent::Claude));
    assert!(!text.contains("Reboot over HTTP."));
}

#[test]
fn a_fix_run_keeps_its_short_prompt() {
    let job = approved_job(Job::new("Add a reboot button", Gates::default()));
    let task = Task::FixCi { log: "boom".into() };
    assert_eq!(
        job_prompt(&job, &task, Expect::Commits, Agent::Claude, &[]),
        prompt(&task, Expect::Commits, Agent::Claude)
    );
}

#[test]
fn a_fix_ci_run_may_end_with_rerun_ci() {
    let stage = Stage::CiAndReview { pr: 0 };
    let fix_ci = Task::FixCi { log: "log".into() };
    let expect = Expect::for_task(&stage, &fix_ci).unwrap();
    assert_eq!(expect, Expect::FixCi);
    assert_eq!(
        Expect::for_task(
            &stage,
            &Task::Revise {
                feedback: "x".into()
            }
        ),
        Some(Expect::Commits)
    );
    assert_eq!(
        event_from_result(
            expect,
            &result(Some(
                json!({"outcome": "rerun_ci", "reason": "the runner lost its network"})
            ))
        ),
        Event::Agent(AgentResult::RerunCi {
            reason: "the runner lost its network".into()
        })
    );
}

#[test]
fn rerun_ci_fails_a_run_that_is_not_fix_ci() {
    let output = json!({"outcome": "rerun_ci", "reason": "flaky"});
    for expect in [Expect::Clarify, Expect::Spec, Expect::Plan, Expect::Commits] {
        let reason = failure_reason(event_from_result(expect, &result(Some(output.clone()))));
        assert!(reason.contains("\"rerun_ci\""), "{reason}");
    }
}

#[test]
fn rerun_ci_needs_a_reason() {
    for output in [
        json!({"outcome": "rerun_ci"}),
        json!({"outcome": "rerun_ci", "reason": "  "}),
    ] {
        let reason = failure_reason(event_from_result(Expect::FixCi, &result(Some(output))));
        assert!(reason.contains("no reason"), "{reason}");
    }
}

#[test]
fn only_the_fix_ci_schema_lists_rerun_ci() {
    let schema = Expect::FixCi.schema();
    assert_eq!(
        schema["properties"]["outcome"]["enum"],
        json!(["questions", "commits_ready", "rerun_ci"])
    );
    assert!(schema["properties"]["reason"].is_object());
    assert!(schema["properties"]["pull_request"].is_object());
    let commits = Expect::Commits.schema();
    assert_eq!(
        commits["properties"]["outcome"]["enum"],
        json!(["questions", "commits_ready"])
    );
    assert!(commits["properties"].get("reason").is_none());
}

#[test]
fn the_fix_ci_prompt_says_when_to_rerun() {
    let text = prompt(
        &Task::FixCi { log: "boom".into() },
        Expect::FixCi,
        Agent::Claude,
    );
    assert!(text.contains("boom"));
    assert!(text.contains("\"rerun_ci\""));
    assert!(text.contains("\"commits_ready\""));
}

/// A job whose only pull request is open as #`number` and being watched, so
/// a `StartReview` moves it to a review turn.
fn watching(number: u64) -> Job {
    let mut job = approved_job(Job::new("Add a reboot button", Gates::default()));
    for event in [
        Event::Approve,
        Event::Agent(AgentResult::CommitsReady(ready())),
        Event::ChecksPassed,
        Event::Published { number },
    ] {
        job.apply(event).unwrap();
    }
    job
}

fn review_failure_reason(event: Event) -> String {
    match event {
        Event::ReviewFailed { reason } => reason,
        other => panic!("expected ReviewFailed, got {other:?}"),
    }
}

#[test]
fn a_review_task_expects_a_review_verdict() {
    let expect = Expect::for_task(&Stage::CiAndReview { pr: 0 }, &Task::Review);
    assert_eq!(expect, Some(Expect::Review));
}

#[test]
fn the_review_schema_matches_the_bundled_schema() {
    let schema = Expect::Review.schema();
    assert_eq!(
        schema["properties"]["outcome"]["enum"],
        json!(["approve", "changes"])
    );
    assert!(schema["properties"]["feedback"].is_object());
    assert_eq!(schema["required"], json!(["outcome"]));
    assert!(schema["properties"].get("questions").is_none());

    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("agents/schemas/review.json");
    let bundled: Value = serde_json::from_str(&fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(schema, bundled, "the run's schema drifted from the skill's");
}

#[test]
fn the_review_prompt_reviews_the_branch_and_keeps_the_worker_read_only() {
    let text = prompt(&Task::Review, Expect::Review, Agent::Claude);
    assert!(text.contains("\"approve\""));
    assert!(text.contains("\"changes\""));
    assert!(text.to_lowercase().contains("skill"));
    assert!(text.contains("Do not change any files"));
    assert!(text.contains("do not push"));
}

#[test]
fn a_prompt_names_the_rules_file_the_agent_reads() {
    let pr = PlannedPr {
        title: "maestrod: add a reboot endpoint".into(),
        summary: "POST /reboot".into(),
        number: None,
        description: None,
        notes: None,
    };
    let claude = prompt(
        &Task::Implement { pr: pr.clone() },
        Expect::Commits,
        Agent::Claude,
    );
    assert!(claude.contains("CLAUDE.md"));
    assert!(!claude.contains("AGENTS.md"));

    let codex = prompt(&Task::Implement { pr }, Expect::Commits, Agent::Codex);
    assert!(codex.contains("AGENTS.md"));
    assert!(!codex.contains("CLAUDE.md"));
}

#[test]
fn the_review_prompt_names_the_method_for_each_agent() {
    // Claude loads the factory's review skill through --plugin-dir, so its
    // prompt names the skill. Codex has no such flag, so its prompt names the
    // skill's path to keep it off a reviewed repository's own `review` skill.
    let claude = prompt(&Task::Review, Expect::Review, Agent::Claude);
    assert!(claude.contains("CLAUDE.md"));
    assert!(!claude.contains("/factory-agents/skills/review/SKILL.md"));

    let codex = prompt(&Task::Review, Expect::Review, Agent::Codex);
    assert!(codex.contains("AGENTS.md"));
    assert!(codex.contains("/factory-agents/skills/review/SKILL.md"));
}

const MCP_PARAGRAPH: &str = "This run has these MCP servers: `a`, `b`. Before you start, load the \
     tools each one exposes and read what they do, so you know them when a later step needs one. \
     Some servers are gateways: the tools they expose search for and run other tools that are not \
     listed up front. For such a server, the loaded tools are not all it offers. When a step \
     needs an outside app or data source, ask the server's search tool for it before you decide \
     no tool exists, and run what it finds through the server's own execute tool.";

#[test]
fn the_mcp_paragraph_names_the_servers_for_a_stage_prompt() {
    let job = approved_job(Job::new("Add a reboot button", Gates::default()));
    let task = Task::Implement {
        pr: job.plan()[0].clone(),
    };
    let servers = ["a".to_owned(), "b".to_owned()];
    for agent in [Agent::Claude, Agent::Codex, Agent::Copilot] {
        let text = job_prompt(&job, &task, Expect::Commits, agent, &servers);
        assert!(
            text.contains(MCP_PARAGRAPH),
            "the {agent:?} stage prompt is missing the MCP paragraph"
        );
    }
}

#[test]
fn the_mcp_paragraph_names_the_servers_for_the_review_prompt() {
    let mut job = watching(7);
    job.apply(start_review()).unwrap();
    let servers = ["a".to_owned(), "b".to_owned()];
    for agent in [Agent::Claude, Agent::Codex, Agent::Copilot] {
        let text = job_prompt(&job, &Task::Review, Expect::Review, agent, &servers);
        assert!(
            text.contains(MCP_PARAGRAPH),
            "the {agent:?} review prompt is missing the MCP paragraph"
        );
    }
}

const NIX_PARAGRAPH: &str = "Nix is installed in this environment, with flakes enabled. When a \
     tool or binary the work needs is missing, try it from nixpkgs before you conclude it is \
     unavailable, for example `nix shell nixpkgs#<package> -c <command>`. The repository's own \
     instructions still decide how to build and test: if they name a dev shell or a command, use \
     that. This is for tools that neither the repository's setup nor this environment provides.";

#[test]
fn every_prompt_carries_the_nix_paragraph_once() {
    for needle in ["nix shell", "nixpkgs", "nix shell nixpkgs#"] {
        assert!(NIX_PARAGRAPH.contains(needle));
    }
    assert!(NIX_PARAGRAPH.contains("The repository's own instructions still decide"));

    let job = approved_job(Job::new("Add a reboot button", Gates::default()));
    let implement = Task::Implement {
        pr: job.plan()[0].clone(),
    };
    let mut reviewing = watching(7);
    reviewing.apply(start_review()).unwrap();
    let gaps = gaps_task();
    let analyze = Task::AnalyzeFailure { log: "boom".into() };
    let check = Task::CheckDiff {
        diff: "+v = 1".into(),
        body: "Bumps serde".into(),
    };
    let clarify = Task::Clarify {
        request: "Add a reboot button".into(),
    };
    let fix_ci = Task::FixCi { log: "boom".into() };
    let fix_checks = Task::FixChecks {
        report: "boom".into(),
    };
    let answer = Task::Answer {
        questions: vec![Question {
            text: "Which board?".into(),
        }],
        answers: vec!["rpi5".into()],
    };
    let revise = Task::Revise {
        feedback: "x".into(),
    };
    let two = ["a".to_owned(), "b".to_owned()];
    for agent in [Agent::Claude, Agent::Codex, Agent::Copilot] {
        let mut texts = vec![
            prompt(&clarify, Expect::Clarify, agent),
            prompt(&Task::WriteSpec, Expect::Spec, agent),
            prompt(&Task::WritePlan, Expect::Plan, agent),
            prompt(&fix_ci, Expect::FixCi, agent),
            prompt(&fix_checks, Expect::Commits, agent),
            prompt(&answer, Expect::Clarify, agent),
            prompt(&revise, Expect::Commits, agent),
            prompt(&Task::Review, Expect::Review, agent),
            prompt(&analyze, Expect::FailureAnalysis, agent),
            prompt(&check, Expect::DiffCheck, agent),
            prompt(&gaps, Expect::PackagingGaps, agent),
        ];
        for servers in [&two[..], &[]] {
            texts.push(job_prompt(
                &job,
                &implement,
                Expect::Commits,
                agent,
                servers,
            ));
            texts.push(job_prompt(
                &reviewing,
                &Task::Review,
                Expect::Review,
                agent,
                servers,
            ));
        }
        for text in texts {
            assert_eq!(
                text.matches(NIX_PARAGRAPH).count(),
                1,
                "the {agent:?} prompt does not carry the nix paragraph once: {text}"
            );
        }
    }

    let schema = serde_json::to_string_pretty(&Expect::Commits.schema()).unwrap();
    let copilot = job_prompt(&job, &implement, Expect::Commits, Agent::Copilot, &[]);
    let after = copilot.split(NIX_PARAGRAPH).nth(1).unwrap();
    assert!(after.starts_with("\n\nThis agent has no structured-output mode"));
    assert!(after.ends_with(&format!("\n\n{schema}")));
    let with = job_prompt(&job, &implement, Expect::Commits, Agent::Copilot, &two);
    assert!(with.ends_with(&format!("{schema}\n\n{MCP_PARAGRAPH}")));
}

#[test]
fn a_prompt_with_no_mcp_server_gets_no_paragraph() {
    let job = approved_job(Job::new("Add a reboot button", Gates::default()));
    let task = Task::Implement {
        pr: job.plan()[0].clone(),
    };
    let with = job_prompt(
        &job,
        &task,
        Expect::Commits,
        Agent::Claude,
        &["a".to_owned(), "b".to_owned()],
    );
    let without = job_prompt(&job, &task, Expect::Commits, Agent::Claude, &[]);
    assert!(without.contains("Implement this pull request"));
    assert!(!without.contains("MCP servers"));
    assert_eq!(with, format!("{without}\n\n{MCP_PARAGRAPH}"));
    assert!(!MCP_PARAGRAPH.contains("Composio"));
}

#[test]
fn the_first_review_pass_carries_the_request_answers_spec_and_plan() {
    let mut job = watching(7);
    job.apply(start_review()).unwrap();
    let text = run_prompt(&job, Expect::Review);
    assert!(text.contains("Add a reboot button"));
    assert!(text.contains("Q: Which board?\nA: rpi5"));
    assert!(text.contains("Reboot over HTTP."));
    assert!(text.contains("maestrod: add a reboot endpoint (the one to implement now)"));
    assert!(text.contains("are decisions"));
    assert!(!text.contains("earlier version"));
}

/// Runs a changes-requested pass with `feedback` on `job`, then the agent's
/// review fix, so the job waits for CI on the new head.
fn changes_then_fix(job: &mut Job, feedback: &str) {
    job.apply(Event::ReviewChanges {
        feedback: feedback.into(),
    })
    .unwrap();
    job.apply(Event::Agent(AgentResult::CommitsReady(ready())))
        .unwrap();
    job.apply(Event::ChecksPassed).unwrap();
    job.apply(Event::Published { number: 7 }).unwrap();
}

#[test]
fn a_later_review_pass_restates_the_feedback_and_reviews_only_the_new_commits() {
    let mut job = watching(7);
    job.apply(start_review()).unwrap();
    changes_then_fix(&mut job, "guard the empty-slice case");
    job.apply(start_review()).unwrap();

    let text = run_prompt(&job, Expect::Review);
    assert!(text.contains("guard the empty-slice case"));
    assert!(text.contains("commits added since your last review"));
    // The resumed review session already holds the request and the spec.
    assert!(!text.contains("Reboot over HTTP."));
    assert!(!text.contains("Which board?"));
}

#[test]
fn a_later_review_pass_carries_the_answers_given_since_the_last_pass() {
    let mut job = watching(7);
    job.apply(start_review()).unwrap();
    job.apply(Event::ReviewChanges {
        feedback: "add an env var fallback".into(),
    })
    .unwrap();
    // The agent asks the requester instead of following the feedback.
    job.apply(Event::Agent(AgentResult::Questions(vec![Question {
        text: "Keep the spec and drop the fallback?".into(),
    }])))
    .unwrap();
    job.apply(Event::Answer(vec!["yes, keep the spec".into()]))
        .unwrap();
    job.apply(Event::Agent(AgentResult::CommitsReady(ready())))
        .unwrap();
    job.apply(Event::ChecksPassed).unwrap();
    job.apply(Event::Published { number: 7 }).unwrap();
    job.apply(start_review()).unwrap();

    let text = run_prompt(&job, Expect::Review);
    assert!(text.contains("Q: Keep the spec and drop the fallback?\nA: yes, keep the spec"));
    assert!(text.contains("are decisions"));
    // The answer from clarifying went to the first pass already.
    assert!(!text.contains("Which board?"));
}

#[test]
fn a_review_run_reports_the_verdict() {
    assert_eq!(
        event_from_result(Expect::Review, &result(Some(json!({"outcome": "approve"})))),
        Event::ReviewApproved
    );
    assert_eq!(
        event_from_result(
            Expect::Review,
            &result(Some(
                json!({"outcome": "changes", "feedback": "narrow the lock"})
            ))
        ),
        Event::ReviewChanges {
            feedback: "narrow the lock".into()
        }
    );
}

#[test]
fn a_review_run_without_a_valid_result_fails_closed() {
    for output in [
        json!({"outcome": "changes"}),
        json!({"outcome": "changes", "feedback": "  "}),
        json!({"outcome": "approved"}),
        json!({"feedback": "no outcome"}),
    ] {
        let event = event_from_result(Expect::Review, &result(Some(output.clone())));
        assert!(
            matches!(event, Event::ReviewFailed { .. }),
            "{output} gave {event:?}"
        );
    }
    let reason = review_failure_reason(event_from_result(Expect::Review, &result(None)));
    assert!(reason.contains("without structured output"), "{reason}");

    let mut message = result(None);
    message.subtype = "error_during_execution".into();
    message.is_error = true;
    let reason = review_failure_reason(event_from_result(Expect::Review, &message));
    assert!(reason.contains("error_during_execution"), "{reason}");
}

#[test]
fn a_review_worker_passes_the_model_and_gets_no_github_token() {
    let mut run = worker_run(Path::new("/jobs/7"), SessionRun::Resume(SESSION.into()));
    run.expect = Expect::Review;
    run.model = Some("opus".into());
    let args = strings(&podman_args(&WorkerConfig::default(), &run));

    assert_eq!(value_after(&args, "--model"), "opus");
    let schema: Value = serde_json::from_str(value_after(&args, "--json-schema")).unwrap();
    assert_eq!(schema, Expect::Review.schema());
    assert_eq!(value_after(&args, "--env"), "CLAUDE_CODE_OAUTH_TOKEN");
    assert!(!args.iter().any(|arg| arg == "--secret"));
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("GH_TOKEN") || arg.contains("GITHUB_TOKEN")),
        "a review worker gets no GitHub credentials"
    );
}

#[test]
fn renovate_runs_get_no_github_credentials() {
    for expect in [
        Expect::FailureAnalysis,
        Expect::DiffCheck,
        Expect::PackagingGaps,
    ] {
        let mut run = worker_run(Path::new("/jobs/7"), SessionRun::Start(SESSION.into()));
        run.expect = expect;
        let args = strings(&podman_args(&WorkerConfig::default(), &run));

        assert_eq!(value_after(&args, "--env"), "CLAUDE_CODE_OAUTH_TOKEN");
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("GH_TOKEN") || arg.contains("GITHUB_TOKEN")),
            "a Renovate run gets no GitHub credentials"
        );
    }
}

#[tokio::test]
async fn a_review_run_that_dies_without_a_result_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    fs::write(&empty, "").unwrap();
    let config = WorkerConfig {
        podman: fake_podman(dir.path(), &empty, "the worker crashed", 1),
        ..WorkerConfig::default()
    };
    let mut worker = worker_run(dir.path(), SessionRun::Start(SESSION.into()));
    worker.expect = Expect::Review;

    let report = run(&config, &worker, |_| {}, |_| {}, |_| {}).await;
    let reason = review_failure_reason(report.event);
    assert!(reason.contains("the worker crashed"), "{reason}");
}

/// A [`Event::StartReview`] on a head no earlier pass read, so the reviewer
/// runs.
fn start_review() -> Event {
    static HEADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = HEADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Event::StartReview {
        head: format!("head{n}"),
    }
}

#[test]
fn the_renovate_tasks_expect_their_verdicts_in_the_renovate_stage() {
    let analyze = Task::AnalyzeFailure { log: "boom".into() };
    let check = Task::CheckDiff {
        diff: "d".into(),
        body: "b".into(),
    };
    assert_eq!(
        Expect::for_task(&Stage::Renovate, &analyze),
        Some(Expect::FailureAnalysis)
    );
    assert_eq!(
        Expect::for_task(&Stage::Renovate, &check),
        Some(Expect::DiffCheck)
    );
    assert_eq!(
        Expect::FailureAnalysis.outcomes(),
        ["transient", "caused_by_update"]
    );
    assert_eq!(Expect::DiffCheck.outcomes(), ["safe", "unsafe"]);
}

#[test]
fn the_renovate_schemas_require_a_reason_and_forbid_questions() {
    let failure = Expect::FailureAnalysis.schema();
    assert_eq!(failure["required"], json!(["outcome", "reason"]));
    assert_eq!(
        failure["properties"]["outcome"]["enum"],
        json!(["transient", "caused_by_update"])
    );
    assert!(failure["properties"]["questions"].is_null());

    let diff = Expect::DiffCheck.schema();
    assert_eq!(diff["required"], json!(["outcome", "reason", "summary"]));
    assert_eq!(
        diff["properties"]["outcome"]["enum"],
        json!(["safe", "unsafe"])
    );
    for key in ["from", "to"] {
        assert_eq!(diff["properties"][key]["type"], "string");
    }

    let strict = Expect::DiffCheck.strict_schema();
    assert_eq!(strict["required"].as_array().unwrap().len(), 5);
    assert_eq!(
        strict["properties"]["from"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(strict["properties"]["outcome"]["type"], "string");
}

#[test]
fn a_failure_analysis_reports_its_verdict() {
    use factory_runner::event_from_output;
    assert_eq!(
        event_from_output(
            Expect::FailureAnalysis,
            Some(json!({"outcome": "transient", "reason": "registry timeout"}))
        ),
        Event::Agent(AgentResult::FailureVerdict {
            transient: true,
            reason: "registry timeout".into()
        })
    );
    assert_eq!(
        event_from_output(
            Expect::FailureAnalysis,
            Some(json!({"outcome": "caused_by_update", "reason": "API removed"}))
        ),
        Event::Agent(AgentResult::FailureVerdict {
            transient: false,
            reason: "API removed".into()
        })
    );
    for output in [
        json!({"outcome": "transient"}),
        json!({"outcome": "transient", "reason": " "}),
        json!({"outcome": "questions", "questions": ["why?"]}),
        json!({"outcome": "safe", "reason": "x"}),
    ] {
        let event = event_from_output(Expect::FailureAnalysis, Some(output.clone()));
        assert!(
            matches!(event, Event::AgentFailed { .. }),
            "{output} gave {event:?}"
        );
    }
    assert!(matches!(
        event_from_output(Expect::FailureAnalysis, None),
        Event::AgentFailed { .. }
    ));
}

#[test]
fn a_diff_check_reports_its_verdict_and_the_refs() {
    use factory_runner::event_from_output;
    assert_eq!(
        event_from_output(
            Expect::DiffCheck,
            Some(json!({
                "outcome": "safe",
                "reason": "only Cargo.lock changes",
                "summary": "serde 1.0.1 to 1.0.2",
                "from": "v1.0.1",
                "to": "v1.0.2"
            }))
        ),
        Event::Agent(AgentResult::DiffVerdict(Box::new(DiffVerdict {
            safe: true,
            reason: "only Cargo.lock changes".into(),
            summary: "serde 1.0.1 to 1.0.2".into(),
            from: Some("v1.0.1".into()),
            to: Some("v1.0.2".into())
        })))
    );
    // Codex writes null for the refs it leaves out, and an empty string reads as absent.
    assert_eq!(
        event_from_output(
            Expect::DiffCheck,
            Some(json!({
                "outcome": "unsafe",
                "reason": "edits a workflow",
                "summary": "bump",
                "from": null,
                "to": ""
            }))
        ),
        Event::Agent(AgentResult::DiffVerdict(Box::new(DiffVerdict {
            safe: false,
            reason: "edits a workflow".into(),
            summary: "bump".into(),
            from: None,
            to: None
        })))
    );
    for output in [
        json!({"outcome": "safe", "reason": "r"}),
        json!({"outcome": "safe", "summary": "s"}),
        json!({"outcome": "approve", "reason": "r", "summary": "s"}),
    ] {
        let event = event_from_output(Expect::DiffCheck, Some(output.clone()));
        assert!(
            matches!(event, Event::AgentFailed { .. }),
            "{output} gave {event:?}"
        );
    }
}

#[test]
fn the_renovate_prompts_carry_their_input_and_keep_the_worker_read_only() {
    let analyze = Task::AnalyzeFailure {
        log: "test foo failed".into(),
    };
    let check = Task::CheckDiff {
        diff: "+version = 1.2.3".into(),
        body: "Bumps serde".into(),
    };
    for agent in [Agent::Claude, Agent::Codex] {
        let text = prompt(&analyze, Expect::FailureAnalysis, agent);
        assert!(text.contains("test foo failed"));
        assert!(text.contains("\"transient\"") && text.contains("\"caused_by_update\""));
        assert!(text.contains("Do not change any files"));
        assert!(!text.contains("Moon Skywalker"));

        let text = prompt(&check, Expect::DiffCheck, agent);
        assert!(text.contains("+version = 1.2.3") && text.contains("Bumps serde"));
        assert!(text.contains("changes only dependency or packaging version files"));
        assert!(text.contains("\"safe\"") && text.contains("\"unsafe\""));
        assert!(text.contains("Do not change any files"));
        assert!(!text.contains("Moon Skywalker"));
    }
}

fn gaps_task() -> Task {
    Task::FindGaps {
        context: Box::new(factory_core::PackagingContext {
            kind: "snap".into(),
            upstream_repo: "up/app".into(),
            pr_title: "Update app".into(),
            pr_body: "Bumps app".into(),
            pr_diff: "+app 2".into(),
            manifests: vec![factory_core::FileText {
                path: "snap/snapcraft.yaml".into(),
                text: Some("name: app".into()),
            }],
            from: Some("v1".into()),
            to: Some("v2".into()),
            upstream: vec![factory_core::FileText {
                path: "CMakeLists.txt".into(),
                text: Some("+option(NEW_FLAG)".into()),
            }],
            existing: vec![factory_core::ExistingGap {
                request: "Add the old flag\n\nd".into(),
                dismissed: false,
            }],
        }),
    }
}

#[test]
fn the_gap_analysis_has_a_schema_and_parses_its_result() {
    use factory_runner::event_from_output;
    let schema = Expect::PackagingGaps.schema();
    assert_eq!(schema["properties"]["outcome"]["enum"], json!(["gaps"]));
    let item = &schema["properties"]["gaps"]["items"];
    for field in ["title", "description", "evidence", "current", "suggestion"] {
        assert!(item["properties"][field].is_object(), "{field}");
    }
    let strict = Expect::PackagingGaps.strict_schema();
    assert_eq!(
        strict["properties"]["gaps"]["items"]["additionalProperties"],
        json!(false)
    );

    let gap = json!({
        "title": "Add NEW_FLAG", "description": "d", "evidence": "e",
        "current": "c", "suggestion": "s"
    });
    assert_eq!(
        event_from_output(
            Expect::PackagingGaps,
            Some(json!({"outcome": "gaps", "gaps": [gap]}))
        ),
        Event::Agent(AgentResult::Gaps(vec![factory_core::Gap {
            title: "Add NEW_FLAG".into(),
            description: "d".into(),
            evidence: "e".into(),
            current: "c".into(),
            suggestion: "s".into(),
        }]))
    );
    assert_eq!(
        event_from_output(
            Expect::PackagingGaps,
            Some(json!({"outcome": "gaps", "gaps": []}))
        ),
        Event::Agent(AgentResult::Gaps(Vec::new()))
    );
    for output in [
        json!({"outcome": "gaps"}),
        json!({"outcome": "gaps", "gaps": [{"title": "", "description": "d", "evidence": "e", "current": "c", "suggestion": "s"}]}),
        json!({"outcome": "questions", "questions": ["why"]}),
    ] {
        let event = event_from_output(Expect::PackagingGaps, Some(output.clone()));
        assert!(
            matches!(event, Event::AgentFailed { .. }),
            "{output} gave {event:?}"
        );
    }
}

#[test]
fn the_gap_prompt_carries_the_input_and_the_rules_of_its_type() {
    for agent in [Agent::Claude, Agent::Codex] {
        let text = prompt(&gaps_task(), Expect::PackagingGaps, agent);
        for needle in [
            "name: app",
            "+app 2",
            "+option(NEW_FLAG)",
            "Add the old flag",
            "cmake-parameters",
            "stage-packages",
            "internal refactors",
            "Do not change any files",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
        assert!(!text.contains("Moon Skywalker"));
    }
    let Task::FindGaps { mut context } = gaps_task() else {
        unreachable!()
    };
    context.kind = "deb".into();
    context.from = None;
    context.to = None;
    let text = prompt(
        &Task::FindGaps { context },
        Expect::PackagingGaps,
        Agent::Claude,
    );
    assert!(text.contains("Build-Depends") && text.contains("debian/patches"));
    assert!(text.contains("unavailable"));
}
