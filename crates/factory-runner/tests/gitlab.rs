mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_core::PrDescription;
use factory_runner::{
    Access, Account, Ci, FailedCheck, Forge, ForgeAccount, ForgeKind, GitLab, LookupError, PrState,
    RebaseCheckbox, Rerun, gitlab_job_id,
};

const TOKEN: &str = "glpat-secret";

/// A `glab` that records `host|token|arguments` for each call and answers by
/// the method (`$3`) and endpoint (`$4`) in `body`. A call that sends a body
/// on stdin leaves it in `stdin`.
fn fake_glab(dir: &Path, body: &str) -> (GitLab, PathBuf) {
    let calls = dir.join("calls");
    let stdin = dir.join("stdin");
    let glab = support::executable(
        dir,
        "glab",
        &format!(
            "#!/bin/bash\n[ \"${{@: -2:1}}\" = --hostname ] && [ \"${{@: -1}}\" = \"$GITLAB_HOST\" ] || {{ echo 'glab: no --hostname' >&2; exit 2; }}\n[ \"$GLAB_ENABLE_CI_AUTOLOGIN\" = false ] || exit 2\nargs=\"$*\"\nprintf '%s|%s|%s|%s\\n' \"$GITLAB_HOST\" \"${{GITLAB_TOKEN-none}}\" \"${{GLAB_CONFIG_DIR:+config}}\" \"${{args% --hostname *}}\" >> '{}'\ncase \"$*\" in *'=@-'*) cat > '{}' ;; esac\n{body}\n",
            calls.display(),
            stdin.display(),
        ),
    );
    (
        GitLab {
            glab,
            settle_polls: 1,
            settle_interval: Duration::ZERO,
            ..GitLab::default()
        },
        calls,
    )
}

fn calls(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn account() -> Account {
    Account {
        token: Some(TOKEN.into()),
        login: "deb-bot".into(),
        name: "deb-bot".into(),
        email: "7-deb-bot@users.noreply.salsa.debian.org".into(),
    }
}

fn description() -> PrDescription {
    PrDescription {
        title: "foo: add a flag".into(),
        body: "## Why\n\nBecause.\n".into(),
    }
}

const P: &str = "projects/team%2Fsub%2Fpkg";

#[test]
fn salsa_is_a_forge_kind_with_its_own_urls_and_repository_form() {
    assert_eq!(ForgeKind::parse("salsa"), Some(ForgeKind::Salsa));
    assert_eq!(ForgeKind::Salsa.as_str(), "salsa");
    assert_eq!(ForgeKind::Salsa.display_name(), "Salsa");
    assert_eq!(ForgeKind::Salsa.host(), "salsa.debian.org");
    assert!(ForgeKind::FIXED.contains(&ForgeKind::Salsa));
    assert_eq!(
        ForgeKind::Salsa.web_url("team/sub/pkg"),
        "https://salsa.debian.org/team/sub/pkg"
    );
    assert_eq!(
        ForgeKind::Salsa.pr_url("team/sub/pkg", 9),
        "https://salsa.debian.org/team/sub/pkg/-/merge_requests/9"
    );
    assert_eq!(
        ForgeKind::GitHub.pr_url("o/r", 9),
        "https://github.com/o/r/pull/9"
    );

    assert!(ForgeKind::Salsa.valid_repo("team/pkg"));
    assert!(ForgeKind::Salsa.valid_repo("python-team/packages/foo"));
    for bad in [
        "pkg",
        "team//pkg",
        "team/",
        "/pkg",
        "team/../pkg",
        "team/p kg",
    ] {
        assert!(!ForgeKind::Salsa.valid_repo(bad), "{bad}");
    }
    assert!(ForgeKind::GitHub.valid_repo("o/r"));
    assert!(!ForgeKind::GitHub.valid_repo("o/r/x"));
}

#[tokio::test]
async fn every_call_names_the_host_and_token_in_the_environment_only() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(dir.path(), r#"echo '{"iid":7,"state":"merged"}'"#);
    let status = glab
        .status(&account(), "team/sub/pkg", 7, &[])
        .await
        .unwrap();
    assert_eq!(status.state, PrState::Merged);
    let none = Account {
        token: None,
        ..account()
    };
    glab.status(&none, "team/sub/pkg", 7, &[]).await.unwrap();

    let log = calls(&log);
    assert_eq!(log.len(), 2);
    assert_eq!(
        log[0],
        format!("salsa.debian.org|{TOKEN}|config|api --method GET {P}/merge_requests/7")
    );
    assert!(
        log[1].starts_with("salsa.debian.org|none|config|"),
        "{}",
        log[1]
    );
    assert!(!log[0].rsplit('|').next().unwrap().contains(TOKEN));
}

#[tokio::test]
async fn lookup_account_reads_the_user_and_builds_the_salsa_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"if [ "$4" = user ] && [ "$GITLAB_TOKEN" = tok-good ]; then
  echo '{"id":42,"username":"deb-bot"}'
elif [ "$GITLAB_TOKEN" = tok-boom ]; then
  echo "glab: boom tok-boom" >&2; exit 1
else
  echo "glab: 401 Unauthorized (HTTP 401)" >&2; exit 1
fi"#,
    );
    let found = glab.lookup_account("tok-good").await.unwrap();
    assert_eq!(
        found,
        ForgeAccount {
            login: "deb-bot".into(),
            id: 42
        }
    );
    assert_eq!(
        glab.lookup_account("tok-bad").await,
        Err(LookupError::Rejected)
    );
    let Err(LookupError::Failed(message)) = glab.lookup_account("tok-boom").await else {
        panic!("expected a failed lookup");
    };
    assert!(!message.contains("tok-boom"), "{message}");

    let account = glab.account("tok-good".into(), &found);
    assert_eq!(account.login, "deb-bot");
    assert_eq!(account.name, "deb-bot");
    assert_eq!(account.email, "42-deb-bot@users.noreply.salsa.debian.org");
}

#[test]
fn git_gets_the_clone_url_and_an_oauth2_basic_header() {
    let glab = GitLab::default();
    assert_eq!(
        glab.clone_url("team/sub/pkg"),
        "https://salsa.debian.org/team/sub/pkg.git"
    );
    let header = glab.git_auth_header(&account()).unwrap();
    // base64 of "oauth2:glpat-secret"
    assert_eq!(header, "Authorization: Basic b2F1dGgyOmdscGF0LXNlY3JldA==");
    let none = Account {
        token: None,
        ..account()
    };
    assert_eq!(glab.git_auth_header(&none), None);
}

#[tokio::test]
async fn access_takes_the_higher_membership_level() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$4" in
  projects/o%2Fowner) echo '{"permissions":{"project_access":null,"group_access":{"access_level":50}}}' ;;
  projects/o%2Fmaint) echo '{"permissions":{"project_access":{"access_level":40},"group_access":null}}' ;;
  projects/o%2Fdev) echo '{"permissions":{"project_access":{"access_level":30},"group_access":{"access_level":20}}}' ;;
  projects/o%2Frep) echo '{"permissions":{"project_access":{"access_level":20},"group_access":null}}' ;;
  projects/o%2Fguest) echo '{"permissions":{"project_access":{"access_level":10},"group_access":null}}' ;;
  projects/o%2Fpublic) echo '{"permissions":{"project_access":null,"group_access":null}}' ;;
  projects/o%2Fhidden) echo "glab: 404 Project Not Found (HTTP 404)" >&2; exit 1 ;;
  *) echo "glab: boom" >&2; exit 1 ;;
esac"#,
    );
    let account = account();
    let access = |repo: &'static str| {
        let glab = &glab;
        let account = &account;
        async move { glab.access(account, repo).await }
    };
    assert_eq!(access("o/owner").await, Ok(Access::Admin));
    assert_eq!(access("o/maint").await, Ok(Access::Admin));
    assert_eq!(access("o/dev").await, Ok(Access::Write));
    assert_eq!(access("o/rep").await, Ok(Access::Read));
    assert_eq!(access("o/guest").await, Ok(Access::Read));
    assert_eq!(access("o/public").await, Ok(Access::Read));
    assert_eq!(access("o/hidden").await, Ok(Access::NotVisible));
    assert!(access("o/other").await.is_err());
}

#[tokio::test]
async fn may_merge_asks_the_merge_request_and_never_for_read() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */merge_requests/1) echo '{"iid":1,"user":{"can_merge":true}}' ;;
  */merge_requests/2) echo '{"iid":2,"user":{"can_merge":false}}' ;;
esac"#,
    );
    let account = account();
    let may = |number: u64, access: Access| {
        let glab = &glab;
        let account = &account;
        async move {
            glab.may_merge(account, "team/sub/pkg", number, access)
                .await
                .unwrap()
        }
    };
    assert!(may(1, Access::Write).await);
    assert!(may(1, Access::Admin).await);
    assert!(!may(2, Access::Write).await);
    assert!(!may(1, Access::Read).await);
    assert!(!may(1, Access::NotVisible).await);
    assert_eq!(calls(&log).len(), 3);
}

const MR: &str = r#"{"iid":7,"state":"opened","sha":"abc","has_conflicts":false,"detailed_merge_status":"ci_still_running","head_pipeline":{"id":99,"sha":"abc","status":"failed","web_url":"https://salsa.debian.org/team/sub/pkg/-/pipelines/99"}}"#;
const JOBS: &str = r#"[
  {"id":1,"name":"build","status":"success","web_url":"https://salsa.debian.org/team/sub/pkg/-/jobs/1"},
  {"id":2,"name":"lint","status":"failed","allow_failure":false,"web_url":"https://salsa.debian.org/team/sub/pkg/-/jobs/2"},
  {"id":3,"name":"flaky","status":"failed","allow_failure":true,"web_url":"https://salsa.debian.org/team/sub/pkg/-/jobs/3"},
  {"id":4,"name":"deploy","status":"manual","web_url":"https://salsa.debian.org/team/sub/pkg/-/jobs/4"}
]"#;
const NOTES: &str = r#"[
  {"body":"assigned to alice","system":true,"author":{"username":"alice"},"created_at":"2026-11-20T10:00:00.000Z"},
  {"body":"Please rename the flag.","system":false,"author":{"username":"alice"},"created_at":"2026-11-20T11:00:00.100Z"},
  {"body":"Opened by the factory.","author":{"username":"deb-bot"},"created_at":"2026-11-20T11:30:00.000Z"},
  {"body":"token bot","author":{"username":"project_5_bot_9f3a"},"created_at":"2026-11-20T11:40:00.000Z"},
  {"body":"flagged bot","author":{"username":"ci-helper","bot":true},"created_at":"2026-11-20T11:50:00.000Z"},
  {"body":"Use a constant here.","author":{"username":"alice"},"created_at":"2026-11-20T12:00:00.000Z",
   "position":{"new_path":"src/a.rs","new_line":42,"old_path":"src/a.rs","old_line":null}},
  {"body":"On a removed line.","author":{"username":"alice"},"created_at":"2026-11-20T12:05:00.000Z",
   "position":{"new_path":"src/b.rs","new_line":null,"old_path":"src/b.rs","old_line":9}}
]"#;

fn status_script() -> String {
    format!(
        "case \"$4\" in\n  */merge_requests/7) echo '{MR}' ;;\n  */pipelines/99/jobs) echo '{JOBS}' ;;\n  */merge_requests/7/notes) echo '{NOTES}' ;;\nesac"
    )
}

#[tokio::test]
async fn status_reads_pipeline_jobs_and_human_notes() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(dir.path(), &status_script());
    let status = glab
        .status(&account(), "team/sub/pkg", 7, &["deb-bot".into()])
        .await
        .unwrap();
    assert_eq!(status.state, PrState::Open);
    assert_eq!(status.head, "abc");
    assert!(!status.conflicting);
    assert_eq!(
        status.ci,
        Ci::Failed(vec![FailedCheck {
            name: "lint".into(),
            url: Some("https://salsa.debian.org/team/sub/pkg/-/jobs/2".into()),
        }])
    );
    let comments: Vec<(&str, Option<&str>, &str)> = status
        .comments
        .iter()
        .map(|c| (c.body.as_str(), c.location.as_deref(), c.at.as_str()))
        .collect();
    assert_eq!(
        comments,
        [
            ("Please rename the flag.", None, "2026-11-20T11:00:00.100Z"),
            (
                "Use a constant here.",
                Some("src/a.rs:42"),
                "2026-11-20T12:00:00.000Z"
            ),
            (
                "On a removed line.",
                Some("src/b.rs:9"),
                "2026-11-20T12:05:00.000Z"
            ),
        ]
    );
}

#[tokio::test]
async fn a_merge_request_without_a_pipeline_on_its_head_has_no_checks() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */merge_requests/1) echo '{"iid":1,"state":"opened","sha":"new","head_pipeline":null}' ;;
  */merge_requests/2) echo '{"iid":2,"state":"opened","sha":"new","head_pipeline":{"id":5,"sha":"old","status":"success"}}' ;;
  */merge_requests/3) echo '{"iid":3,"state":"opened","sha":"new","has_conflicts":true,"head_pipeline":{"id":6,"sha":"new","status":"failed","web_url":"https://salsa.debian.org/o/r/-/pipelines/6"}}' ;;
  */pipelines/6/jobs) echo '[]' ;;
  */notes) echo '[]' ;;
esac"#,
    );
    let ci = |number: u64| {
        let glab = &glab;
        async move {
            glab.status(&account(), "o/r", number, &[])
                .await
                .unwrap()
                .ci
        }
    };
    assert_eq!(ci(1).await, Ci::NoChecks);
    assert_eq!(ci(2).await, Ci::NoChecks);
    assert!(
        !calls(&log).iter().any(|line| line.contains("pipelines/5")),
        "a stale pipeline is not read"
    );
    // A pipeline that failed without making a job still fails the checks.
    let status = glab.status(&account(), "o/r", 3, &[]).await.unwrap();
    assert!(status.conflicting);
    assert_eq!(
        status.ci,
        Ci::Failed(vec![FailedCheck {
            name: "pipeline".into(),
            url: Some("https://salsa.debian.org/o/r/-/pipelines/6".into()),
        }])
    );
}

#[tokio::test]
async fn a_merged_merge_request_reads_no_pipeline_or_notes() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"echo '{"iid":7,"state":"closed","sha":"abc"}'"#,
    );
    let status = glab.status(&account(), "o/r", 7, &[]).await.unwrap();
    assert_eq!(status.state, PrState::Closed);
    assert_eq!(calls(&log).len(), 1);
}

#[tokio::test]
async fn open_pr_creates_a_squash_merge_request_that_removes_its_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$3 $4" in
  GET*merge_requests) echo '[]' ;;
  POST*merge_requests) echo '{"iid":12}' ;;
esac"#,
    );
    let number = glab
        .open_pr(
            &account(),
            "team/sub/pkg",
            "main",
            "factory/job-7-pr-1",
            &description(),
        )
        .await
        .unwrap();
    assert_eq!(number, 12);
    let log = calls(&log);
    let post = log.last().unwrap();
    assert!(
        post.contains(&format!("api --method POST {P}/merge_requests ")),
        "{post}"
    );
    for field in [
        "source_branch=factory/job-7-pr-1",
        "target_branch=main",
        "title=foo: add a flag",
        "description=@-",
        "remove_source_branch=true",
        "squash=true",
    ] {
        assert!(post.contains(field), "{field} missing from {post}");
    }
    assert!(!post.contains("Because."), "the body goes on stdin: {post}");
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        description().body
    );
    assert!(log[0].contains("source_branch=factory/job-7-pr-1"));
    assert!(log[0].contains("state=opened"));
}

#[tokio::test]
async fn open_pr_edits_the_open_merge_request_and_reruns_failed_checks() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        &format!(
            "case \"$3 $4\" in\n  GET*merge_requests) echo '[{{\"iid\":6,\"project_id\":5,\"source_project_id\":9,\"target_branch\":\"main\"}},{{\"iid\":8,\"project_id\":5,\"source_project_id\":5,\"target_branch\":\"other\"}},{{\"iid\":7,\"project_id\":5,\"source_project_id\":5,\"target_branch\":\"main\"}}]' ;;\n  PUT*) echo '{{\"iid\":7}}' ;;\n  POST*retry) echo '{{}}' ;;\n  GET*/merge_requests/7) echo '{MR}' ;;\n  GET*/pipelines/99/jobs) echo '{JOBS}' ;;\n  GET*/notes) echo '[]' ;;\nesac"
        ),
    );
    let number = glab
        .open_pr(
            &account(),
            "team/sub/pkg",
            "main",
            "factory/job-7-pr-1",
            &description(),
        )
        .await
        .unwrap();
    assert_eq!(number, 7);
    let log = calls(&log);
    assert!(
        log.iter().any(|line| line.ends_with(&format!(
            "api --method PUT {P}/merge_requests/7 -f title=foo: add a flag -F description=@-"
        ))),
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|line| line.ends_with(&format!("api --method POST {P}/pipelines/99/retry"))),
        "{log:?}"
    );
}

#[tokio::test]
async fn rerun_failed_retries_the_failed_jobs_of_the_head_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        &format!(
            "{}\n",
            status_script().replace("esac", "  */retry) echo '{}' ;;\nesac")
        ),
    );
    let rerun = glab
        .rerun_failed(&account(), "team/sub/pkg", 7)
        .await
        .unwrap();
    assert_eq!(rerun, Rerun::Started(vec![2, 3]));
    let retries: Vec<String> = calls(&log)
        .into_iter()
        .filter(|line| line.contains("retry"))
        .collect();
    assert_eq!(retries.len(), 1);
    assert!(retries[0].ends_with(&format!("api --method POST {P}/pipelines/99/retry")));
}

#[tokio::test]
async fn rerun_failed_has_nothing_to_rerun_without_a_failed_job_or_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */merge_requests/1) echo '{"iid":1,"sha":"a","head_pipeline":{"id":5,"sha":"a","status":"success"}}' ;;
  */merge_requests/2) echo '{"iid":2,"sha":"a","head_pipeline":null}' ;;
  */pipelines/5/jobs) echo '[{"id":1,"name":"build","status":"success"}]' ;;
esac"#,
    );
    for number in [1, 2] {
        assert_eq!(
            glab.rerun_failed(&account(), "o/r", number).await.unwrap(),
            Rerun::NothingToRerun
        );
    }
    assert!(!calls(&log).iter().any(|line| line.contains("POST")));
}

#[tokio::test]
async fn merge_squashes_and_removes_the_source_branch_and_merge_head_binds_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(dir.path(), "echo '{}'");
    glab.merge(&account(), "team/sub/pkg", 7).await.unwrap();
    glab.merge_head(&account(), "team/sub/pkg", 7, "abc")
        .await
        .unwrap();
    let log = calls(&log);
    assert!(log[0].ends_with(&format!(
        "api --method PUT {P}/merge_requests/7/merge -F squash=true -F should_remove_source_branch=true"
    )));
    assert!(
        log[1].ends_with("-F should_remove_source_branch=true -f sha=abc"),
        "{}",
        log[1]
    );
}

#[tokio::test]
async fn comment_and_approve_send_the_body_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(dir.path(), "echo '{}'");
    glab.comment(&account(), "team/sub/pkg", 7, "Looks risky.")
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        "Looks risky."
    );
    glab.approve(&account(), "team/sub/pkg", 7, "abc", "Safe update.")
        .await
        .unwrap();
    let log = calls(&log);
    assert!(log[0].ends_with(&format!(
        "api --method POST {P}/merge_requests/7/notes -F body=@-"
    )));
    assert!(log[1].ends_with(&format!(
        "api --method POST {P}/merge_requests/7/approve -f sha=abc"
    )));
    assert!(log[2].ends_with("/merge_requests/7/notes -F body=@-"));
    assert!(!log.iter().any(|line| line.contains("Safe update.")));
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        "Safe update."
    );
}

#[tokio::test]
async fn merged_pr_finds_the_merged_merge_request_of_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$*" in *state=merged*) echo '[{"iid":30,"project_id":5,"source_project_id":9},{"iid":31,"project_id":5,"source_project_id":5}]' ;; *) echo '[]' ;; esac"#,
    );
    assert_eq!(
        glab.merged_pr(&account(), "team/sub/pkg", "factory/job-1")
            .await,
        Ok(Some(31))
    );
    let line = &calls(&log)[0];
    assert!(line.contains("source_branch=factory/job-1"), "{line}");
}

#[tokio::test]
async fn renovate_prs_need_an_author_and_list_open_merge_requests_against_the_base() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"echo '[{"iid":5,"title":"Update dep","sha":"aaa","web_url":"https://salsa.debian.org/team/sub/pkg/-/merge_requests/5"}]'"#,
    );
    let err = glab
        .renovate_prs(&account(), "team/sub/pkg", "main", None)
        .await
        .unwrap_err();
    assert!(err.contains("no fixed Renovate account"), "{err}");
    assert_eq!(calls(&log), Vec::<String>::new());

    let prs = glab
        .renovate_prs(&account(), "team/sub/pkg", "main", Some("renovate-bot"))
        .await
        .unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].number, 5);
    assert_eq!(prs[0].head, "aaa");
    assert_eq!(
        prs[0].url,
        "https://salsa.debian.org/team/sub/pkg/-/merge_requests/5"
    );
    let line = &calls(&log)[0];
    for field in [
        "state=opened",
        "target_branch=main",
        "author_username=renovate-bot",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
}

#[tokio::test]
async fn a_listing_reads_every_page() {
    let dir = tempfile::tempdir().unwrap();
    let full: Vec<String> = (1..=100)
        .map(|n| format!(r#"{{"iid":{n},"title":"t","sha":"s"}}"#))
        .collect();
    let (glab, log) = fake_glab(
        dir.path(),
        &format!(
            "case \"$*\" in\n  *page=1\\ *) echo '[{}]' ;;\n  *) echo '[{{\"iid\":101,\"title\":\"t\",\"sha\":\"s\"}}]' ;;\nesac",
            full.join(",")
        ),
    );
    let prs = glab
        .renovate_prs(&account(), "o/r", "main", Some("r"))
        .await
        .unwrap();
    assert_eq!(prs.len(), 101);
    assert_eq!(calls(&log).len(), 2);
}

#[tokio::test]
async fn pr_details_builds_a_unified_diff_and_refuses_one_gitlab_cut() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */merge_requests/7) echo '{"iid":7,"title":"Update dep","description":"Body","sha":"abc"}' ;;
  */merge_requests/7/versions) echo '[{"id":3,"head_commit_sha":"abc","created_at":"2026-11-20T10:00:00.000Z"}]' ;;
  */merge_requests/7/versions/3) echo '{"head_commit_sha":"abc","state":"collected","real_size":"2","diffs":[
    {"old_path":"a.txt","new_path":"a.txt","new_file":false,"deleted_file":false,"diff":"@@ -1 +1 @@\n-old\n+new\n"},
    {"old_path":"b.txt","new_path":"b.txt","new_file":true,"deleted_file":false,"diff":"@@ -0,0 +1 @@\n+added"}
  ]}' ;;
  */merge_requests/8) echo '{"iid":8,"sha":"def"}' ;;
  */merge_requests/8/versions) echo '[{"id":4,"head_commit_sha":"def","created_at":"2026-11-20T10:00:00.000Z"}]' ;;
  */merge_requests/8/versions/4) echo '{"head_commit_sha":"def","state":"collected","real_size":"1","diffs":[{"old_path":"big","new_path":"big","too_large":true,"diff":""}]}' ;;
esac"#,
    );
    let details = glab.pr_details(&account(), "o/r", 7).await.unwrap();
    assert_eq!(details.title, "Update dep");
    assert_eq!(details.body, "Body");
    assert_eq!(details.head, "abc");
    assert_eq!(details.files, ["a.txt", "b.txt"]);
    assert_eq!(
        details.diff,
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n\
         diff --git a/b.txt b/b.txt\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+added\n"
    );
    let info = glab.pr_info(&account(), "o/r", 7).await.unwrap();
    assert_eq!(info.files, ["a.txt", "b.txt"]);
    let err = glab.pr_details(&account(), "o/r", 8).await.unwrap_err();
    assert!(err.contains("too large"), "{err}");
}

#[tokio::test]
async fn file_at_reads_the_raw_file_and_maps_404_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */repository/files/debian%2Fcontrol/raw) printf 'Source: pkg\n' ;;
  *) echo "glab: 404 File Not Found (HTTP 404)" >&2; exit 1 ;;
esac"#,
    );
    assert_eq!(
        glab.file_at(&account(), "team/sub/pkg", "debian/control", "main")
            .await,
        Ok(Some("Source: pkg\n".into()))
    );
    assert_eq!(
        glab.file_at(&account(), "team/sub/pkg", "nope", "main")
            .await,
        Ok(None)
    );
    assert!(calls(&log)[0].contains("-f ref=main"));
}

#[tokio::test]
async fn compare_patch_returns_the_patch_of_the_matching_file() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$*" in
  *from=v1*to=v2*) echo '{"compare_timeout":false,"diffs":[
    {"old_path":"go.mod","new_path":"go.mod","diff":"@@ -1 +1 @@\n-a\n+b\n"},
    {"old_path":"x","new_path":"x","diff":""}]}' ;;
  *) echo "glab: 404 Not Found (HTTP 404)" >&2; exit 1 ;;
esac"#,
    );
    let patch = |path: &'static str| {
        let glab = &glab;
        async move {
            glab.compare_patch(&account(), "o/r", "v1", "v2", path)
                .await
        }
    };
    assert_eq!(
        patch("go.mod").await,
        Ok(Some("@@ -1 +1 @@\n-a\n+b\n".into()))
    );
    assert_eq!(patch("x").await, Ok(None));
    assert_eq!(patch("other").await, Ok(None));
    let err = glab
        .compare_patch(&account(), "o/r", "v1", "v9", "go.mod")
        .await
        .unwrap_err();
    assert!(err.contains("not a valid comparison"), "{err}");
}

#[test]
fn compare_range_reads_gitlab_compare_links_of_the_upstream() {
    let glab = GitLab::default();
    let body =
        "Release notes: [diff](https://salsa.debian.org/Team/Up/-/compare/v1.0...v1.1?w=1) end";
    assert_eq!(
        glab.compare_range(body, "team/up"),
        Some(("v1.0".into(), "v1.1".into()))
    );
    assert_eq!(glab.compare_range(body, "team/other"), None);
    assert_eq!(
        glab.compare_range("https://github.com/team/up/compare/a...b", "team/up"),
        None
    );
}

#[tokio::test]
async fn head_commit_dates_the_head_by_the_first_push_that_made_it_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */merge_requests/7) echo '{"iid":7,"sha":"abc"}' ;;
  */merge_requests/7/versions) echo '[
    {"head_commit_sha":"abc","created_at":"2026-11-20T10:00:05.500Z"},
    {"head_commit_sha":"abc","created_at":"2026-11-20T09:00:00.000Z"},
    {"head_commit_sha":"old","created_at":"2026-11-19T09:00:00.000Z"}]' ;;
  */repository/commits/abc) echo '{"committed_date":"2026-11-20T08:59:00.000Z"}' ;;
esac"#,
    );
    let head = glab.head_commit(&account(), "o/r", 7).await.unwrap();
    assert_eq!(head.sha, "abc");
    assert_eq!(head.pushed_at.as_deref(), Some("2026-11-20 09:00:00"));
    assert_eq!(head.committed_at.as_deref(), Some("2026-11-20 08:59:00"));
}

#[tokio::test]
async fn merged_at_reads_the_merge_time() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */merge_requests/1) echo '{"iid":1,"merged_at":"2026-11-20T10:00:05.500Z"}' ;;
  *) echo '{"iid":2,"merged_at":null}' ;;
esac"#,
    );
    assert_eq!(
        glab.merged_at(&account(), "o/r", 1).await,
        Ok(Some("2026-11-20 10:00:05".into()))
    );
    assert_eq!(glab.merged_at(&account(), "o/r", 2).await, Ok(None));
}

#[tokio::test]
async fn request_renovate_rebase_ticks_the_description_of_the_current_head() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"echo '{"iid":7,"sha":"abc","description":"Update\n\n- [ ] <!-- rebase-check -->Rebase"}'"#,
    );
    assert_eq!(
        glab.request_renovate_rebase(&account(), "o/r", 7, "old")
            .await,
        Ok(RebaseCheckbox::HeadMoved)
    );
    assert_eq!(
        glab.request_renovate_rebase(&account(), "o/r", 7, "abc")
            .await,
        Ok(RebaseCheckbox::Ticked)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        "Update\n\n- [x] <!-- rebase-check -->Rebase"
    );
    assert!(
        calls(&log)
            .last()
            .unwrap()
            .ends_with("api --method PUT projects/o%2Fr/merge_requests/7 -F description=@-")
    );
}

#[test]
fn a_job_id_comes_from_a_gitlab_job_url() {
    assert_eq!(
        gitlab_job_id("https://salsa.debian.org/team/sub/pkg/-/jobs/42"),
        Some("42")
    );
    assert_eq!(
        gitlab_job_id("https://salsa.debian.org/t/p/-/jobs/42?x=1"),
        Some("42")
    );
    assert_eq!(
        gitlab_job_id("https://salsa.debian.org/t/p/-/pipelines/42"),
        None
    );
    assert_eq!(
        gitlab_job_id("https://salsa.debian.org/t/p/-/jobs/abc"),
        None
    );
}

#[tokio::test]
async fn failure_report_ends_each_failed_jobs_trace() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */jobs/2/trace) printf 'compiling\nerror: lint failed\n' ;;
  */jobs/3/trace) echo "glab: 500 boom (HTTP 500)" >&2; exit 1 ;;
esac"#,
    );
    let checks = [
        FailedCheck {
            name: "lint".into(),
            url: Some("https://salsa.debian.org/team/sub/pkg/-/jobs/2".into()),
        },
        FailedCheck {
            name: "flaky".into(),
            url: Some("https://salsa.debian.org/team/sub/pkg/-/jobs/3".into()),
        },
        FailedCheck {
            name: "pipeline".into(),
            url: Some("https://salsa.debian.org/team/sub/pkg/-/pipelines/9".into()),
        },
    ];
    let report = glab
        .failure_report(&account(), "team/sub/pkg", &checks)
        .await
        .unwrap();
    assert!(report.contains("Check \"lint\" failed.\ncompiling\nerror: lint failed"));
    assert!(report.contains("Could not fetch its log"), "{report}");
    assert!(report.contains("Details: https://salsa.debian.org/team/sub/pkg/-/pipelines/9"));
    assert_eq!(calls(&log).len(), 2);
}

#[tokio::test]
async fn a_long_trace_keeps_only_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        "echo START; head -c 40000 /dev/zero | tr '\\0' x; echo END",
    );
    let checks = [FailedCheck {
        name: "lint".into(),
        url: Some("https://salsa.debian.org/o/r/-/jobs/2".into()),
    }];
    let report = glab
        .failure_report(&account(), "o/r", &checks)
        .await
        .unwrap();
    assert!(!report.contains("START"));
    assert!(report.contains("END"));
}

#[tokio::test]
async fn a_failure_never_carries_the_token_and_a_refusal_is_marked() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"echo "boom: token $GITLAB_TOKEN" >&2; exit 1"#,
    );
    let err = glab.merge(&account(), "o/r", 15).await.unwrap_err();
    assert!(err.starts_with("glab api PUT"), "{err}");
    assert!(!err.contains(TOKEN), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        "echo 'glab: 401 Unauthorized (HTTP 401)' >&2; exit 1",
    );
    let err = glab.merge(&account(), "o/r", 15).await.unwrap_err();
    assert!(err.starts_with(factory_runner::REJECTED), "{err}");
}

#[tokio::test]
async fn only_a_successful_pipeline_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */notes) echo '[]' ;;
  */merge_requests/*) n=${4##*/}
    echo "{\"iid\":$n,\"state\":\"opened\",\"sha\":\"a\",\"head_pipeline\":{\"id\":$n,\"sha\":\"a\",\"status\":\"$(case $n in 1) echo success;; 2) echo manual;; 3) echo skipped;; 4) echo running;; esac)\"}}" ;;
  */pipelines/1/jobs) echo '[{"id":1,"name":"build","status":"success"}]' ;;
  */pipelines/2/jobs) echo '[{"id":1,"name":"build","status":"success"},{"id":2,"name":"deploy","status":"manual","allow_failure":false}]' ;;
  */pipelines/3/jobs) echo '[{"id":1,"name":"build","status":"skipped"}]' ;;
  */pipelines/4/jobs) echo '[{"id":1,"name":"build","status":"success"}]' ;;
esac"#,
    );
    let ci = |number: u64| {
        let glab = &glab;
        async move {
            glab.status(&account(), "o/r", number, &[])
                .await
                .unwrap()
                .ci
        }
    };
    assert_eq!(ci(1).await, Ci::Passed);
    assert_eq!(ci(2).await, Ci::Pending);
    assert_eq!(ci(3).await, Ci::NoChecks);
    assert_eq!(ci(4).await, Ci::Pending);
}

#[tokio::test]
async fn a_fork_pipeline_is_read_and_retried_in_its_own_project() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        r#"case "$4" in
  projects/o%2Fr/merge_requests/7) echo '{"iid":7,"state":"opened","sha":"a","head_pipeline":{"id":50,"project_id":77,"sha":"a","status":"failed"}}' ;;
  projects/77/pipelines/50/jobs) echo '[{"id":9,"name":"lint","status":"failed","web_url":"https://salsa.debian.org/fork/r/-/jobs/9"}]' ;;
  projects/77/pipelines/50/retry) echo '{}' ;;
  projects/o%2Fr/merge_requests/7/notes) echo '[]' ;;
  projects/fork%2Fr/jobs/9/trace) echo 'fork trace' ;;
esac"#,
    );
    let status = glab.status(&account(), "o/r", 7, &[]).await.unwrap();
    let Ci::Failed(checks) = status.ci else {
        panic!("expected failed checks");
    };
    let report = glab
        .failure_report(&account(), "o/r", &checks)
        .await
        .unwrap();
    assert!(report.contains("fork trace"), "{report}");
    assert_eq!(
        glab.rerun_failed(&account(), "o/r", 7).await.unwrap(),
        Rerun::Started(vec![9])
    );
    assert!(
        calls(&log)
            .iter()
            .any(|line| line.ends_with("api --method POST projects/77/pipelines/50/retry"))
    );
}

#[tokio::test]
async fn a_fork_merge_request_with_the_same_branch_is_not_ours() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$3 $4" in
  GET*merge_requests) echo '[{"iid":3,"project_id":5,"source_project_id":9,"target_branch":"main"}]' ;;
  POST*merge_requests) echo '{"iid":12}' ;;
esac"#,
    );
    let number = glab
        .open_pr(&account(), "o/r", "main", "factory/job-1", &description())
        .await
        .unwrap();
    assert_eq!(number, 12);
    assert_eq!(
        glab.merged_pr(&account(), "o/r", "factory/job-1").await,
        Ok(None)
    );
}

#[tokio::test]
async fn only_a_collected_diff_of_the_reviewed_head_is_reviewed() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        r#"case "$4" in
  */merge_requests/7) echo '{"iid":7,"sha":"a"}' ;;
  */merge_requests/7/versions) echo '[{"id":1,"head_commit_sha":"a","created_at":"2026-11-20T10:00:00.000Z"}]' ;;
  */merge_requests/7/versions/1) echo '{"head_commit_sha":"a","state":"overflow","diffs":[{"old_path":"a","new_path":"a","diff":""}]}' ;;
  */merge_requests/8) echo '{"iid":8,"sha":"a"}' ;;
  */merge_requests/8/versions) echo '[{"id":2,"head_commit_sha":"older","created_at":"2026-11-20T10:00:00.000Z"}]' ;;
  */merge_requests/10) echo '{"iid":10,"sha":"a"}' ;;
  */merge_requests/10/versions) echo '[{"id":5,"head_commit_sha":"a","created_at":"2026-11-20T10:00:00.000Z"}]' ;;
  */merge_requests/10/versions/5) echo '{"head_commit_sha":"a","state":"collected","real_size":"1"}' ;;
  */merge_requests/11) echo '{"iid":11,"sha":"a"}' ;;
  */merge_requests/11/versions) echo '[{"id":6,"head_commit_sha":"a","created_at":"2026-11-20T10:00:00.000Z"}]' ;;
  */merge_requests/11/versions/6) echo '{"head_commit_sha":"a","state":"collected","real_size":"2","diffs":[{"old_path":"a","new_path":"a","diff":""}]}' ;;
  */merge_requests/9) echo '{"iid":9,"sha":"a"}' ;;
  */merge_requests/9/versions) echo '[{"id":3,"head_commit_sha":"a","created_at":"2026-11-20T10:00:00.000Z"}]' ;;
  */merge_requests/9/versions/3) echo '{"head_commit_sha":"b","state":"collected","real_size":"1","diffs":[{"old_path":"a","new_path":"a","diff":""}]}' ;;
esac"#,
    );
    for number in [7, 8, 9, 10, 11] {
        let err = glab
            .pr_details(&account(), "o/r", number)
            .await
            .unwrap_err();
        assert!(err.contains("cannot be reviewed"), "{err}");
        assert!(glab.pr_info(&account(), "o/r", number).await.is_err());
    }
}

#[test]
fn ambient_overrides_do_not_change_the_call() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "ambient_run"])
        .envs([
            ("JOB_TOKEN", "ambient"),
            ("CI_JOB_TOKEN", "ambient"),
            ("GIT_DIR", "/nonexistent"),
            ("GITLAB_ACCESS_TOKEN", "ambient"),
            ("API_PROTOCOL", "http"),
            ("GLAB_API_PROTOCOL", "http"),
            ("GLAB_IS_OAUTH2", "true"),
            ("OAUTH2_REFRESH_TOKEN", "ambient"),
            ("GLAB_SKIP_TLS_VERIFY", "true"),
            ("SKIP_TLS_VERIFY", "true"),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
}

/// Run by `ambient_overrides_do_not_change_the_call` with the overrides set,
/// and with the binary named by a relative path.
#[tokio::test]
#[ignore = "runs in a child process with ambient overrides"]
async fn ambient_run() {
    let dir = tempfile::tempdir().unwrap();
    let (mut glab, log) = fake_glab(
        dir.path(),
        r#"[ -z "$JOB_TOKEN$CI_JOB_TOKEN$API_PROTOCOL$GIT_DIR$GITLAB_ACCESS_TOKEN$GLAB_IS_OAUTH2$OAUTH2_REFRESH_TOKEN$GLAB_SKIP_TLS_VERIFY$SKIP_TLS_VERIFY" ] || { echo leaked >&2; exit 2; }
[ "$GLAB_API_PROTOCOL" = https ] && [ "$GITLAB_API_PROTOCOL" = https ] || exit 2
echo '{"iid":7,"state":"merged"}'"#,
    );
    let cwd = std::env::current_dir().unwrap();
    glab.glab = pathdiff(&glab.glab, &cwd);
    assert!(glab.glab.is_relative());
    let none = Account {
        token: None,
        ..account()
    };
    let status = glab.status(&none, "o/r", 7, &[]).await.unwrap();
    assert_eq!(status.state, PrState::Merged);
    assert_eq!(calls(&log).len(), 1);
}

/// `path` relative to `base`, both absolute.
fn pathdiff(path: &Path, base: &Path) -> PathBuf {
    let up = base.components().count();
    let mut out = PathBuf::new();
    for _ in 0..up {
        out.push("..");
    }
    out.join(path.strip_prefix("/").unwrap())
}

const MR_JSON: &str = r#"{"iid":12,"title":"Add a flag","description":"Why","state":"opened","sha":"abc","web_url":"https://salsa.debian.org/team/pkg/-/merge_requests/12","source_branch":"flag","target_branch":"main","draft":true,"source_project_id":9,"target_project_id":5,"project_id":5,"author":{"username":"ana"}}"#;

#[tokio::test]
async fn an_empty_search_reads_one_page_of_the_latest_open_merge_requests() {
    let dir = tempfile::tempdir().unwrap();
    let rows: Vec<String> = (1..=21)
        .map(|n| {
            let source = if n == 2 { 9 } else { 5 };
            format!(
                r#"{{"iid":{n},"title":"t","state":"opened","source_branch":"b{n}","target_branch":"main","source_project_id":{source},"target_project_id":5,"web_url":"u","author":{{"username":"bob"}}}}"#
            )
        })
        .collect();
    let (glab, log) = fake_glab(dir.path(), &format!("echo '[{}]'", rows.join(",")));
    let prs = glab
        .search_open_prs(&account(), "team/pkg", "")
        .await
        .unwrap();
    assert_eq!(prs.len(), 21);
    assert!(!prs[0].fork && prs[1].fork && !prs[0].draft);
    assert_eq!(prs[0].head_branch, "b1");
    assert_eq!(prs[0].author, "bob");
    let calls = calls(&log);
    assert_eq!(calls.len(), 1);
    for field in [
        "state=opened",
        "order_by=updated_at",
        "sort=desc",
        "per_page=21",
    ] {
        assert!(
            calls[0].contains(field),
            "{field} missing from {}",
            calls[0]
        );
    }
    assert!(!calls[0].contains("search="), "{}", calls[0]);
    assert!(!calls[0].contains("page=1"), "{}", calls[0]);
}

#[tokio::test]
async fn a_title_search_asks_gitlab_to_search_titles() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(dir.path(), &format!("echo '[{MR_JSON}]'"));
    let prs = glab
        .search_open_prs(&account(), "team/pkg", "flag")
        .await
        .unwrap();
    assert_eq!(prs.len(), 1);
    assert!(prs[0].fork && prs[0].draft);
    assert_eq!(prs[0].base_branch, "main");
    let calls = calls(&log);
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].contains("search=flag") && calls[0].contains("in=title"),
        "{}",
        calls[0]
    );
}

#[tokio::test]
async fn a_number_finds_that_merge_request_only_when_it_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, log) = fake_glab(
        dir.path(),
        &format!(
            r#"case "$4" in
  */merge_requests/12) echo '{MR_JSON}' ;;
  */merge_requests/13) echo '{}' ;;
  *) echo 'glab: 404 Not Found (HTTP 404)' >&2; exit 1 ;;
esac"#,
            MR_JSON.replace("opened", "merged").replace("12", "13")
        ),
    );
    for query in ["12", "#12"] {
        let prs = glab
            .search_open_prs(&account(), "team/pkg", query)
            .await
            .unwrap();
        assert_eq!(prs.len(), 1, "{query}");
        assert_eq!(prs[0].number, 12);
    }
    assert_eq!(
        glab.search_open_prs(&account(), "team/pkg", "13")
            .await
            .unwrap(),
        Vec::new()
    );
    assert_eq!(
        glab.search_open_prs(&account(), "team/pkg", "99")
            .await
            .unwrap(),
        Vec::new()
    );
    assert_eq!(calls(&log).len(), 4);
}

#[tokio::test]
async fn pull_request_reads_the_source_project_address_and_none_for_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let (glab, _) = fake_glab(
        dir.path(),
        &format!(
            r#"case "$4" in
  */merge_requests/12) echo '{MR_JSON}' ;;
  projects/9) echo '{{"http_url_to_repo":"https://salsa.debian.org/ana/pkg.git"}}' ;;
  *) echo 'glab: 404 Not Found (HTTP 404)' >&2; exit 1 ;;
esac"#
        ),
    );
    let pr = glab
        .pull_request(&account(), "team/pkg", 12)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pr.state, PrState::Open);
    assert!(pr.fork && pr.draft);
    assert_eq!(
        pr.head_repo_clone_url.as_deref(),
        Some("https://salsa.debian.org/ana/pkg.git")
    );
    assert_eq!(
        (pr.head_branch.as_str(), pr.head_sha.as_str()),
        ("flag", "abc")
    );
    assert_eq!(
        (pr.title.as_str(), pr.body.as_str(), pr.author.as_str()),
        ("Add a flag", "Why", "ana")
    );
    assert!(
        glab.pull_request(&account(), "team/pkg", 99)
            .await
            .unwrap()
            .is_none()
    );
}
