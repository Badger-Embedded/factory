use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use factory_core::{Event, PrDescription};
use factory_runner::{
    Ci, FailedCheck, GitHub, Observation, PrState, Rerun, WorkerConfig, actions_job_id,
    checks_args, observe, parse_status, run_checks,
};

const VIEW: &str = r#"{
  "state": "OPEN",
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

/// A test that writes or runs a script holds this lock. A script file that is
/// still open for writing in a forked child cannot be executed, and the child
/// lives until its exec, so a parallel test can make the run fail with
/// "Text file busy".
static SCRIPTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn description() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.\n".into(),
    }
}

#[test]
fn status_reads_state_checks_and_human_comments() {
    let status = parse_status(VIEW, INLINE, "moonthebot").unwrap();
    assert_eq!(status.state, PrState::Open);
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
        parse_status(pending, "", "moonthebot").unwrap().ci,
        Ci::Pending
    );

    let passed = r#"{"state": "OPEN", "statusCheckRollup": [
        {"name": "Lint and test", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "Docs", "status": "COMPLETED", "conclusion": "SKIPPED"}]}"#;
    assert_eq!(
        parse_status(passed, "", "moonthebot").unwrap().ci,
        Ci::Passed
    );

    let merged = r#"{"state": "MERGED"}"#;
    assert_eq!(
        parse_status(merged, "", "moonthebot").unwrap().state,
        PrState::Merged
    );
}

#[test]
fn merge_and_ci_failure_come_before_comments() {
    let mut status = parse_status(VIEW, INLINE, "moonthebot").unwrap();
    assert!(matches!(observe(&status, None), Observation::CiFailed(_)));

    status.state = PrState::Merged;
    assert_eq!(observe(&status, None), Observation::Merged);
    status.state = PrState::Closed;
    assert_eq!(observe(&status, None), Observation::Closed);
}

#[test]
fn only_comments_newer_than_the_watermark_are_delivered() {
    let mut status = parse_status(VIEW, INLINE, "moonthebot").unwrap();
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
    let _serial = SCRIPTS.lock().await;
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
        token: Some("ghp_secret".into()),
        ..GitHub::default()
    };

    let number = github
        .open_pr("o/r", "main", "factory/job-7-pr-1", &description())
        .await
        .unwrap();
    assert_eq!(number, 12);
    assert_eq!(fs::read_to_string(&body).unwrap(), description().body);

    let revised = PrDescription {
        body: "## Why\n\nRevised.\n".into(),
        ..description()
    };
    let number = github
        .open_pr("o/r", "main", "factory/job-7-pr-1", &revised)
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
    let _serial = SCRIPTS.lock().await;
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
        .await;
    assert!(report.contains("Check \"Lint and test\" failed."));
    assert!(report.contains("FAILED (job 135)"));
    assert!(report.contains("Details: https://ci.example/1"));
}

#[test]
fn checks_run_with_bash_in_the_checkout_without_tokens() {
    let _serial = SCRIPTS.blocking_lock();
    let config = WorkerConfig {
        volumes: vec!["factory-nix:/nix".into()],
        ..WorkerConfig::default()
    };
    let args: Vec<String> = checks_args(
        &config,
        "factory-job-7",
        Path::new("/jobs/7/work"),
        "just lint",
    )
    .iter()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect();
    assert!(args.iter().any(|arg| arg == "/jobs/7/work:/work"));
    assert!(args.iter().any(|arg| arg == "factory-nix:/nix"));
    assert_eq!(args[args.len() - 3..], ["bash", "-lc", "just lint"]);
    assert!(!args.iter().any(|arg| arg.contains("secret")));
}

#[tokio::test]
async fn check_results_become_events_and_podman_errors_are_retried() {
    let _serial = SCRIPTS.lock().await;
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
    let mut status = parse_status(VIEW, INLINE, "moonthebot").unwrap();
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
        "moonthebot",
    )
    .unwrap();
    assert_eq!(status.ci, Ci::NoChecks);
    assert_eq!(observe(&status, None), Observation::Nothing);
}

#[tokio::test]
async fn merge_squashes_and_deletes_the_branch() {
    let _serial = SCRIPTS.lock().await;
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
    github.merge("o/r", 15).await.unwrap();
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
    let status = parse_status(&conflicting, INLINE, "moonthebot").unwrap();
    assert!(status.conflicting);
    assert_eq!(observe(&status, None), Observation::Conflicted);

    for mergeable in ["MERGEABLE", "UNKNOWN"] {
        let view = VIEW.replacen(
            "\"state\": \"OPEN\",",
            &format!("\"state\": \"OPEN\", \"mergeable\": \"{mergeable}\","),
            1,
        );
        let status = parse_status(&view, INLINE, "moonthebot").unwrap();
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
      *headRefOid*) echo abc123 ;;
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
    let _serial = SCRIPTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("SUCCESS"), RUNS);

    let rerun = quick(gh).rerun_failed("o/r", 12).await.unwrap();

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
    let _serial = SCRIPTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let runs = r#"[{"databaseId": 45, "conclusion": "success"}]"#;
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("FAILURE"), runs);

    let rerun = quick(gh).rerun_failed("o/r", 12).await.unwrap();

    assert_eq!(rerun, Rerun::NothingToRerun);
    assert!(!fs::read_to_string(&calls).unwrap().contains("run rerun"));
}

#[tokio::test]
async fn rerun_failed_waits_until_the_old_failure_is_gone() {
    let _serial = SCRIPTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("FAILURE"), RUNS);
    // GitHub still reports the failure, so every poll reads it.
    quick(gh).rerun_failed("o/r", 12).await.unwrap();
    let reads = fs::read_to_string(&calls)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("pr view") && l.contains("statusCheckRollup"))
        .count();
    assert_eq!(reads, 3);

    let dir = tempfile::tempdir().unwrap();
    let (gh, calls) = rerun_gh(dir.path(), &check("FAILURE"), &check("SUCCESS"), RUNS);
    quick(gh).rerun_failed("o/r", 12).await.unwrap();
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
        .open_pr("o/r", "main", "factory/job-7-pr-1", &description())
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
    let _serial = SCRIPTS.lock().await;
    assert_eq!(edit_and_count_reruns(&check("FAILURE")).await, 2);
}

#[tokio::test]
async fn editing_a_pr_whose_checks_did_not_fail_reruns_nothing() {
    let _serial = SCRIPTS.lock().await;
    assert_eq!(edit_and_count_reruns(&check("SUCCESS")).await, 0);
    let pending =
        r#"{"state": "OPEN", "statusCheckRollup": [{"name": "x", "status": "IN_PROGRESS"}]}"#;
    assert_eq!(edit_and_count_reruns(pending).await, 0);
    assert_eq!(edit_and_count_reruns(r#"{"state": "OPEN"}"#).await, 0);
}

#[tokio::test]
async fn a_failed_rerun_does_not_fail_the_edit() {
    let _serial = SCRIPTS.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (gh, _) = rerun_gh(dir.path(), &check("FAILURE"), &check("FAILURE"), "not json");
    let number = quick(gh)
        .open_pr("o/r", "main", "factory/job-7-pr-1", &description())
        .await
        .unwrap();
    assert_eq!(number, 12);
}
