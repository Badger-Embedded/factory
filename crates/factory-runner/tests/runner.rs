use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use factory_core::{
    AgentResult, Command, EFFORTS, Event, Gates, Job, PlannedPr, PrDescription, Question, Stage,
    Task,
};
use factory_runner::{
    Expect, LogLine, McpConfig, ResultMessage, SessionRun, StreamItem, WorkerConfig, WorkerRun,
    event_from_result, job_prompt, parse_line, place_images, podman_args, prompt, remove_container,
    run, show_images,
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
        workspace: dir.join("work"),
        claude_dir: dir.join("claude"),
        session,
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

/// A podman stand-in that records its arguments, prints `stdout`, writes
/// `stderr` and exits with `code`.
fn fake_podman(dir: &Path, stdout: &Path, stderr: &str, code: i32) -> PathBuf {
    let script = dir.join("podman");
    let args = dir.join("args");
    fs::write(
        &script,
        format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\nprintf '%s' '{stderr}' >&2\nexit {code}\n",
            args.display(),
            stdout.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
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
    assert_eq!(parse_line(r#"{"type":"system","subtype":"init"}"#), vec![]);
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
            }]),
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
    };
    let text = prompt(&Task::Implement { pr }, Expect::Commits);
    assert!(text.contains("maestrod: add a reboot endpoint"));
    assert!(text.contains("\"commits_ready\""));
    assert!(text.contains("Do not push"));
}

#[test]
fn podman_args_give_the_worker_its_token_limits_and_mounts() {
    let config = WorkerConfig {
        plugin_dir: Some(PathBuf::from("/srv/factory/plugin")),
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
    assert_eq!(
        value_after(&args, "--secret"),
        "claude-token,type=env,target=CLAUDE_CODE_OAUTH_TOKEN"
    );
    for volume in [
        "/jobs/7/work:/work",
        "/jobs/7/claude:/claude",
        "/srv/factory/plugin:/factory-plugin:ro",
        "factory-nix:/nix",
    ] {
        assert!(args.iter().any(|arg| arg == volume), "missing {volume}");
    }
    assert_eq!(value_after(&args, "--session-id"), SESSION);
    assert!(!args.iter().any(|arg| arg == "--resume"));
    assert_eq!(value_after(&args, "--plugin-dir"), "/factory-plugin");
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
async fn a_worker_that_dies_without_a_result_fails_with_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    fs::write(&empty, "").unwrap();
    let config = WorkerConfig {
        podman: fake_podman(
            dir.path(),
            &empty,
            "Error: no secret with name or id \"claude-token\"",
            125,
        ),
        ..WorkerConfig::default()
    };

    let report = run(
        &config,
        &worker_run(dir.path(), SessionRun::Start(SESSION.into())),
        |_| {},
    )
    .await;

    let reason = failure_reason(report.event);
    assert!(reason.contains("125"), "{reason}");
    assert!(reason.contains("no secret with name"), "{reason}");
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
    )
    .await;
    assert!(failure_reason(report.event).contains("could not start"));
}

#[test]
fn a_session_resumes_once_claude_code_wrote_its_transcript() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        SessionRun::for_session(dir.path(), SESSION),
        SessionRun::Start(SESSION.into())
    );

    let project = dir.path().join("projects/-work");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("another-session.jsonl"), "").unwrap();
    assert_eq!(
        SessionRun::for_session(dir.path(), SESSION),
        SessionRun::Start(SESSION.into())
    );

    fs::write(project.join(format!("{SESSION}.jsonl")), "").unwrap();
    assert_eq!(
        SessionRun::for_session(dir.path(), SESSION),
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
    for effort in EFFORTS {
        let args = args_with_effort(Expect::Commits, Some(effort));
        assert_eq!(value_after(&args, "--effort"), effort, "{effort}");
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
        Some(Command::RunAgent { task, .. }) => job_prompt(job, &task, expect),
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
    job_prompt(job, &task, Expect::Commits)
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
    let text = job_prompt(&old, &Task::WritePlan, Expect::Plan);
    assert_eq!(text, prompt(&Task::WritePlan, Expect::Plan));
    assert!(!text.contains("Reboot over HTTP."));
}

#[test]
fn a_fix_run_keeps_its_short_prompt() {
    let job = approved_job(Job::new("Add a reboot button", Gates::default()));
    let task = Task::FixCi { log: "boom".into() };
    assert_eq!(
        job_prompt(&job, &task, Expect::Commits),
        prompt(&task, Expect::Commits)
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
    let text = prompt(&Task::FixCi { log: "boom".into() }, Expect::FixCi);
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
fn the_review_schema_matches_the_plugin_schema() {
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
        .join("plugin/schemas/review.json");
    let plugin: Value = serde_json::from_str(&fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(schema, plugin, "the run's schema drifted from the skill's");
}

#[test]
fn the_review_prompt_reviews_the_branch_and_keeps_the_worker_read_only() {
    let text = prompt(&Task::Review, Expect::Review);
    assert!(text.contains("\"approve\""));
    assert!(text.contains("\"changes\""));
    assert!(text.to_lowercase().contains("skill"));
    assert!(text.contains("Do not change any files"));
    assert!(text.contains("do not push"));
}

#[test]
fn the_first_review_pass_carries_no_earlier_feedback() {
    let mut job = watching(7);
    job.apply(Event::StartReview).unwrap();
    let text = run_prompt(&job, Expect::Review);
    assert_eq!(text, prompt(&Task::Review, Expect::Review));
    assert!(!text.contains("earlier version"));
}

#[test]
fn a_later_review_pass_restates_the_feedback_and_reviews_only_the_new_commits() {
    let mut job = watching(7);
    job.apply(Event::StartReview).unwrap();
    job.apply(Event::ReviewChanges {
        feedback: "guard the empty-slice case".into(),
    })
    .unwrap();
    // The agent addresses the review, the checks pass, and the reviewer runs
    // again on the new head.
    job.apply(Event::Agent(AgentResult::CommitsReady(ready())))
        .unwrap();
    job.apply(Event::ChecksPassed).unwrap();
    job.apply(Event::Published { number: 7 }).unwrap();
    job.apply(Event::StartReview).unwrap();

    let text = run_prompt(&job, Expect::Review);
    assert!(text.contains("guard the empty-slice case"));
    assert!(text.contains("commits added since your last review"));
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
    assert_eq!(
        value_after(&args, "--secret"),
        "claude-token,type=env,target=CLAUDE_CODE_OAUTH_TOKEN"
    );
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("GH_TOKEN") || arg.contains("GITHUB_TOKEN")),
        "a review worker gets no GitHub credentials"
    );
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

    let report = run(&config, &worker, |_| {}).await;
    let reason = review_failure_reason(report.event);
    assert!(reason.contains("the worker crashed"), "{reason}");
}
