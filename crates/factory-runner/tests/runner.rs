use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use factory_core::{AgentResult, Event, PlannedPr, Question, Stage, Task};
use factory_runner::{
    Expect, LogLine, ResultMessage, SessionRun, StreamItem, WorkerConfig, WorkerRun,
    event_from_result, parse_line, podman_args, prompt, remove_container, run,
};
use serde_json::{Value, json};

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
            }]),
        ),
        (
            Expect::Commits,
            json!({"outcome": "commits_ready"}),
            AgentResult::CommitsReady,
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
    ] {
        assert!(args.iter().any(|arg| arg == volume), "missing {volume}");
    }
    assert_eq!(value_after(&args, "--session-id"), SESSION);
    assert!(!args.iter().any(|arg| arg == "--resume"));
    assert_eq!(value_after(&args, "--plugin-dir"), "/factory-plugin");
    assert_eq!(value_after(&args, "--output-format"), "stream-json");
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
