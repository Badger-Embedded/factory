mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response};
use base64::Engine;
use factory_core::PrDescription;
use factory_runner::{
    Access, Account, Ci, FailedCheck, Forge, ForgeAccount, Forgejo, ForgejoVersion, LookupError,
    NOT_PERMITTED, PrState, REJECTED, RebaseCheckbox, Rerun, forgejo_client, forgejo_version,
};
use serde_json::{Value, json};

const TOKEN: &str = "fj-secret-token";
const REPO: &str = "owner/name";

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    /// The path without the sub-path and `/api/v1`.
    path: String,
    host: String,
    query: String,
    auth: Option<String>,
    body: String,
}

#[derive(Clone)]
struct Rep {
    status: u16,
    location: Option<String>,
    body: String,
}

fn ok(body: &Value) -> Rep {
    rep(200, &body.to_string())
}

fn rep(status: u16, body: &str) -> Rep {
    Rep {
        status,
        location: None,
        body: body.to_owned(),
    }
}

fn page(seen: &Seen, items: &Value) -> Rep {
    if seen.query.split('&').any(|pair| pair == "page=1") || !seen.query.contains("page=") {
        return ok(items);
    }
    ok(&match items {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let value = if value.is_array() {
                        json!([])
                    } else {
                        value.clone()
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        _ => json!([]),
    })
}

fn quiet(seen: &Seen) -> Rep {
    if seen.path.ends_with("/status") {
        page(seen, &json!({"statuses": []}))
    } else {
        page(seen, &json!([]))
    }
}

type Reply = Box<dyn Fn(&Seen) -> Rep + Send + Sync>;

struct Shared {
    seen: Mutex<Vec<Seen>>,
    reply: Reply,
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1 << 22).await.unwrap();
    let seen = Seen {
        method: parts.method.to_string(),
        path: parts
            .uri
            .path()
            .split_once("/api/v1")
            .map_or(parts.uri.path(), |(_, path)| path)
            .to_owned(),
        host: parts
            .headers
            .get("host")
            .map(|value| value.to_str().unwrap().to_owned())
            .unwrap_or_default(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        auth: parts
            .headers
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_owned()),
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    let reply = (shared.reply)(&seen);
    shared.seen.lock().unwrap().push(seen);
    let mut response = Response::builder().status(reply.status);
    if let Some(location) = reply.location {
        response = response.header("location", location);
    }
    response.body(Body::from(reply.body)).unwrap()
}

struct Stub {
    forge: Forgejo,
    base: String,
    shared: Arc<Shared>,
}

impl Stub {
    fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    fn requests(&self, method: &str, path: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|seen| seen.method == method && seen.path == path)
            .collect()
    }
}

async fn stub_at(prefix: &str, reply: impl Fn(&Seen) -> Rep + Send + Sync + 'static) -> Stub {
    let shared = Arc::new(Shared {
        seen: Mutex::new(Vec::new()),
        reply: Box::new(reply),
    });
    let server =
        support::serve_tls(Router::new().fallback(handle).with_state(shared.clone())).await;
    let root = reqwest::Certificate::from_pem(server.ca_pem.as_bytes()).unwrap();
    let base = format!("{}{prefix}", server.base);
    let mut forge = Forgejo::new(&base, &[root]).unwrap();
    forge.settle_polls = 3;
    forge.settle_interval = Duration::ZERO;
    Stub {
        forge,
        base,
        shared,
    }
}

async fn serve(reply: impl Fn(&Seen) -> Rep + Send + Sync + 'static) -> Stub {
    stub_at("", reply).await
}

fn account(forge: &Forgejo) -> Account {
    forge.account(
        TOKEN.to_owned(),
        &ForgeAccount {
            login: "factory-bot".to_owned(),
            id: 7,
        },
    )
}

fn pull(head: &str) -> Value {
    json!({
        "number": 3,
        "title": "Add a thing",
        "body": "the body",
        "state": "open",
        "merged": false,
        "mergeable": true,
        "html_url": "https://forge.test/owner/name/pulls/3",
        "user": {"login": "renovate-bot"},
        "head": {"sha": head, "ref": "feature", "label": "feature", "repo_id": 5},
        "base": {"ref": "main", "label": "main", "repo_id": 5},
    })
}

fn job_failed(name: &str, url: &str) -> FailedCheck {
    FailedCheck {
        name: name.to_owned(),
        url: Some(url.to_owned()),
    }
}

#[tokio::test]
async fn every_request_carries_the_token_in_the_authorization_header_only() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/user" => ok(&json!({"login": "factory-bot", "id": 7})),
        "/repos/owner/name" => {
            ok(&json!({"permissions": {"admin": false, "push": true, "pull": true}}))
        }
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/commits/abc/status" => page(seen, &json!({"statuses": []})),
        _ => page(seen, &json!([])),
    })
    .await;
    let account = account(&stub.forge);
    stub.forge.lookup_account(TOKEN).await.unwrap();
    stub.forge.access(&account, REPO).await.unwrap();
    stub.forge.status(&account, REPO, 3, &[]).await.unwrap();
    let seen = stub.seen();
    assert!(seen.len() > 4);
    for request in &seen {
        assert_eq!(
            request.auth.as_deref(),
            Some(&*format!("token {TOKEN}")),
            "{} {}",
            request.method,
            request.path
        );
        assert!(!request.path.contains(TOKEN) && !request.query.contains(TOKEN));
        assert!(!request.body.contains(TOKEN));
    }

    let anonymous = Account {
        token: None,
        ..account
    };
    stub.forge.access(&anonymous, REPO).await.unwrap();
    assert_eq!(stub.seen().last().unwrap().auth, None);
}

#[tokio::test]
async fn a_redirect_to_another_origin_is_not_followed() {
    let other = serve(|_| ok(&json!({"permissions": {"admin": true}}))).await;
    let target = format!("{}/api/v1/repos/owner/name", other.base);
    let stub = serve(move |_| Rep {
        status: 302,
        location: Some(target.clone()),
        body: String::new(),
    })
    .await;
    let err = stub
        .forge
        .access(&account(&stub.forge), REPO)
        .await
        .unwrap_err();
    assert!(err.contains("HTTP 302"), "{err}");
    assert!(!err.contains(TOKEN));
    assert!(other.seen().is_empty(), "the redirect target got a request");
}

#[tokio::test]
async fn the_account_commits_as_the_login_at_the_noreply_host() {
    let stub = stub_at("/forgejo", |_| rep(404, "")).await;
    let account = account(&stub.forge);
    assert_eq!(account.name, "factory-bot");
    assert_eq!(account.email, "factory-bot@noreply.127.0.0.1");
    assert_eq!(
        stub.forge.clone_url(REPO),
        format!("{}/owner/name.git", stub.base)
    );
    let header = stub.forge.git_auth_header(&account).unwrap();
    let basic = header.strip_prefix("Authorization: Basic ").unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(basic)
        .unwrap();
    assert_eq!(decoded, format!("factory-bot:{TOKEN}").into_bytes());
    let anonymous = Account {
        token: None,
        ..account
    };
    assert_eq!(stub.forge.git_auth_header(&anonymous), None);
}

#[tokio::test]
async fn a_base_url_with_a_sub_path_is_used_for_every_call() {
    let stub = stub_at("/forgejo", |seen| {
        if seen.path == "/user" {
            ok(&json!({"login": "me", "id": 1}))
        } else {
            rep(404, "")
        }
    })
    .await;
    let found = stub.forge.lookup_account(TOKEN).await.unwrap();
    assert_eq!(found.login, "me");
}

#[tokio::test]
async fn lookup_account_tells_a_rejected_token_from_a_failure() {
    for status in [401, 403] {
        let stub = serve(move |_| rep(status, r#"{"message":"bad"}"#)).await;
        assert_eq!(
            stub.forge.lookup_account(TOKEN).await,
            Err(LookupError::Rejected)
        );
    }
    let stub = serve(|_| rep(500, &format!("boom {TOKEN} boom"))).await;
    let Err(LookupError::Failed(text)) = stub.forge.lookup_account(TOKEN).await else {
        panic!("expected a failure");
    };
    assert!(text.contains("HTTP 500") && !text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn errors_never_echo_the_token() {
    let stub = serve(|seen| {
        rep(
            500,
            &format!(
                r#"{{"message":"server saw {}"}}"#,
                seen.auth.clone().unwrap()
            ),
        )
    })
    .await;
    let account = account(&stub.forge);
    let errors = [
        stub.forge.access(&account, REPO).await.unwrap_err(),
        stub.forge.merge(&account, REPO, 3).await.unwrap_err(),
        stub.forge.status(&account, REPO, 3, &[]).await.unwrap_err(),
        stub.forge
            .comment(&account, REPO, 3, "hi")
            .await
            .unwrap_err(),
    ];
    for err in errors {
        assert!(err.contains("HTTP 500"), "{err}");
        assert!(!err.contains(TOKEN), "{err}");
        assert!(err.contains("<redacted>"), "{err}");
    }
}

#[tokio::test]
async fn a_rejected_call_starts_with_rejected_and_a_refused_write_with_not_permitted() {
    let stub = serve(|seen| {
        if seen.method == "GET" {
            rep(401, r#"{"message":"token is invalid"}"#)
        } else {
            rep(403, r#"{"message":"no write access"}"#)
        }
    })
    .await;
    let account = account(&stub.forge);
    let err = stub.forge.access(&account, REPO).await.unwrap_err();
    assert!(err.starts_with(REJECTED), "{err}");
    let err = stub
        .forge
        .comment(&account, REPO, 3, "hi")
        .await
        .unwrap_err();
    assert!(err.starts_with(NOT_PERMITTED), "{err}");
    assert!(err.contains("no write access"));
}

#[tokio::test]
async fn access_follows_the_permissions_of_the_repository() {
    for (permissions, expected) in [
        (
            json!({"admin": true, "push": true, "pull": true}),
            Access::Admin,
        ),
        (
            json!({"admin": false, "push": true, "pull": true}),
            Access::Write,
        ),
        (
            json!({"admin": false, "push": false, "pull": true}),
            Access::Read,
        ),
    ] {
        let stub = serve(move |_| ok(&json!({"permissions": permissions}))).await;
        assert_eq!(
            stub.forge
                .access(&account(&stub.forge), REPO)
                .await
                .unwrap(),
            expected
        );
    }
    let stub = serve(|_| rep(404, "")).await;
    assert_eq!(
        stub.forge
            .access(&account(&stub.forge), REPO)
            .await
            .unwrap(),
        Access::NotVisible
    );
    assert!(
        stub.forge
            .access(&account(&stub.forge), "not-a-repo")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn may_merge_reads_the_base_branch_of_the_pull_request() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => {
            let mut pull = pull("abc");
            pull["base"]["ref"] = json!("release/1.0");
            ok(&pull)
        }
        "/repos/owner/name/branches/release/1.0" => ok(&json!({"user_can_merge": true})),
        _ => rep(404, ""),
    })
    .await;
    let account = account(&stub.forge);
    assert!(
        stub.forge
            .may_merge(&account, REPO, 3, Access::Write)
            .await
            .unwrap()
    );
    assert!(
        !stub
            .forge
            .may_merge(&account, REPO, 3, Access::Read)
            .await
            .unwrap()
    );

    let stub = stub_branch_denied().await;
    assert!(
        !stub
            .forge
            .may_merge(&account, REPO, 3, Access::Admin)
            .await
            .unwrap()
    );
}

async fn stub_branch_denied() -> Stub {
    serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        _ => ok(&json!({"user_can_merge": false})),
    })
    .await
}

fn status_stub(statuses: &Value) -> impl Fn(&Seen) -> Rep + Send + Sync + use<> {
    let statuses = statuses.clone();
    move |seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/commits/abc/status" => {
            page(seen, &json!({"state": "x", "statuses": statuses}))
        }
        _ => page(seen, &json!([])),
    }
}

fn context(name: &str, status: &str, url: &str) -> Value {
    json!({"context": name, "status": status, "target_url": url})
}

#[tokio::test]
async fn statuses_map_to_the_ci_state() {
    let stub = serve(status_stub(&json!([
        context("a", "success", ""),
        context("b", "failure", "/owner/name/actions/runs/7/jobs/0"),
        context("c", "pending", ""),
        context("d", "error", "https://ci.example.org/build/1"),
    ])))
    .await;
    let status = stub
        .forge
        .status(&account(&stub.forge), REPO, 3, &[])
        .await
        .unwrap();
    assert_eq!(status.head, "abc");
    assert_eq!(
        status.ci,
        Ci::Failed(vec![
            job_failed(
                "b",
                &format!("{}/owner/name/actions/runs/7/jobs/0", stub.base)
            ),
            job_failed("d", "https://ci.example.org/build/1"),
        ])
    );

    let cases = [
        (vec![("a", "success"), ("b", "pending")], Ci::Pending),
        (vec![("a", "success"), ("b", "success")], Ci::Passed),
        (vec![("a", "success"), ("b", "skipped")], Ci::Passed),
        (vec![("a", "success"), ("b", "warning")], Ci::Passed),
        (vec![("a", "warning")], Ci::NoChecks),
        (vec![("a", "skipped")], Ci::NoChecks),
        (vec![], Ci::NoChecks),
    ];
    for (statuses, expected) in cases {
        let statuses: Vec<Value> = statuses
            .into_iter()
            .map(|(name, state)| context(name, state, ""))
            .collect();
        let stub = serve(status_stub(&json!(statuses))).await;
        let status = stub
            .forge
            .status(&account(&stub.forge), REPO, 3, &[])
            .await
            .unwrap();
        assert_eq!(status.ci, expected, "{statuses:?}");
    }
}

#[tokio::test]
async fn a_pull_request_in_each_state_reads_as_that_state() {
    for (state, merged, expected) in [
        ("open", false, PrState::Open),
        ("closed", true, PrState::Merged),
        ("closed", false, PrState::Closed),
    ] {
        let stub = serve(move |seen| {
            if seen.path == "/repos/owner/name/pulls/3" {
                let mut pull = pull("abc");
                pull["state"] = json!(state);
                pull["merged"] = json!(merged);
                ok(&pull)
            } else {
                quiet(seen)
            }
        })
        .await;
        let status = stub
            .forge
            .status(&account(&stub.forge), REPO, 3, &[])
            .await
            .unwrap();
        assert_eq!(status.state, expected);
    }
}

#[tokio::test]
async fn a_pull_request_that_is_still_being_checked_is_not_conflicting() {
    let reads = Arc::new(Mutex::new(0));
    let counter = reads.clone();
    let settling = serve(move |seen| {
        if seen.path == "/repos/owner/name/pulls/3" {
            let mut count = counter.lock().unwrap();
            *count += 1;
            let mut pull = pull("abc");
            pull["mergeable"] = json!(*count > 1);
            ok(&pull)
        } else {
            quiet(seen)
        }
    })
    .await;
    let status = settling
        .forge
        .status(&account(&settling.forge), REPO, 3, &[])
        .await
        .unwrap();
    assert!(!status.conflicting);
    assert_eq!(status.ci, Ci::NoChecks);
}

fn comments_stub() -> impl Fn(&Seen) -> Rep + Send + Sync {
    |seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/issues/3/comments" => page(
            seen,
            &json!([
                null,
                {"body": "from reviewer", "created_at": "2026-11-23T21:00:00+01:00", "user": {"login": "reviewer"}},
                {"body": "from the bot", "created_at": "2026-11-23T10:00:00Z", "user": {"login": "Factory-Bot"}},
            ]),
        ),
        "/repos/owner/name/pulls/3/reviews" => page(
            seen,
            &json!([
                {"id": 1, "body": "please change", "state": "REQUEST_CHANGES", "submitted_at": "2026-11-23T19:59:00.123Z", "comments_count": 3, "user": {"login": "reviewer"}},
                {"id": 2, "body": "", "state": "APPROVED", "submitted_at": "2026-11-23T20:00:00Z", "comments_count": 0, "user": {"login": "other"}},
                {"id": 3, "body": "mine", "state": "COMMENT", "submitted_at": "2026-11-23T20:00:00Z", "comments_count": 1, "user": {"login": "project-bot"}},
            ]),
        ),
        "/repos/owner/name/pulls/3/reviews/1/comments" => ok(&json!([
            {"body": "on the new line", "created_at": "2026-11-23T20:30:00-02:00", "path": "src/a.rs", "position": 12, "original_position": 4, "user": {"login": "reviewer"}},
            {"body": "on the old line", "created_at": "2026-11-23T20:31:00Z", "path": "src/b.rs", "position": 0, "original_position": 9, "user": {"login": "reviewer"}},
            {"body": "on the file", "created_at": "2026-11-23T20:32:00Z", "path": "src/c.rs", "position": 0, "original_position": 0, "user": {"login": "reviewer"}},
        ])),
        "/repos/owner/name/pulls/3/reviews/3/comments" => ok(&json!([
            {"body": "ignored", "created_at": "2026-11-23T20:33:00Z", "path": "src/d.rs", "position": 1, "user": {"login": "project-bot"}},
        ])),
        _ => quiet(seen),
    }
}

#[tokio::test]
async fn comments_carry_their_location_and_utc_time_and_skip_ignored_logins() {
    let stub = serve(comments_stub()).await;
    let ignore = ["factory-bot".to_owned(), "project-bot".to_owned()];
    let status = stub
        .forge
        .status(&account(&stub.forge), REPO, 3, &ignore)
        .await
        .unwrap();
    let got: Vec<(&str, &str, Option<&str>)> = status
        .comments
        .iter()
        .map(|c| (c.at.as_str(), c.body.as_str(), c.location.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("2026-11-23 19:59:00", "please change", None),
            ("2026-11-23 20:00:00", "from reviewer", None),
            ("2026-11-23 20:31:00", "on the old line", Some("src/b.rs:9")),
            ("2026-11-23 20:32:00", "on the file", Some("src/c.rs")),
            (
                "2026-11-23 22:30:00",
                "on the new line",
                Some("src/a.rs:12")
            ),
        ]
    );
    assert_eq!(status.comments[0].author, "reviewer");
}

#[tokio::test]
async fn times_convert_across_a_day_and_year_boundary() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => {
            let mut pull = pull("abc");
            pull["merged_at"] = json!("2026-01-01T00:30:00+01:00");
            ok(&pull)
        }
        _ => rep(404, ""),
    })
    .await;
    assert_eq!(
        stub.forge
            .merged_at(&account(&stub.forge), REPO, 3)
            .await
            .unwrap(),
        Some("2025-12-31 23:30:00".to_owned())
    );
    let stub = serve(|_| ok(&pull("abc"))).await;
    assert_eq!(
        stub.forge
            .merged_at(&account(&stub.forge), REPO, 3)
            .await
            .unwrap(),
        None
    );
}

fn rerun_stub(jobs_status: &'static str) -> impl Fn(&Seen) -> Rep + Send + Sync {
    move |seen| match (seen.method.as_str(), seen.path.as_str()) {
        ("GET", "/repos/owner/name/pulls/3") => ok(&pull("abc")),
        ("GET", "/repos/owner/name/actions/runs") => {
            assert!(seen.query.contains("head_sha=abc"), "{}", seen.query);
            page(
                seen,
                &json!({"total_count": 2, "workflow_runs": [
                    {"id": 41, "index_in_repo": 7, "status": "failure"},
                    {"id": 42, "index_in_repo": 8, "status": "success"},
                ]}),
            )
        }
        ("GET", "/repos/owner/name/actions/runs/41/jobs") => ok(&json!([
            {"id": 500, "name": "lint", "status": jobs_status, "attempt": 1},
            {"id": 501, "name": "test", "status": "success", "attempt": 1},
        ])),
        ("POST", "/repos/owner/name/actions/jobs/500/rerun") => rep(204, ""),
        ("GET", "/repos/owner/name/actions/runs/42/jobs") => ok(&json!([
            {"id": 600, "name": "docs", "status": "success", "attempt": 1},
        ])),
        _ => quiet(seen),
    }
}

#[tokio::test]
async fn rerun_failed_reruns_each_failed_job() {
    let stub = serve(rerun_stub("failure")).await;
    let rerun = stub
        .forge
        .rerun_failed(&account(&stub.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!(rerun, Rerun::Started(vec![41]));
    assert_eq!(
        stub.requests("POST", "/repos/owner/name/actions/jobs/500/rerun")
            .len(),
        1
    );
    assert!(stub.seen().iter().all(|seen| !seen.path.contains("/501/")));
}

#[tokio::test]
async fn rerun_failed_reads_the_jobs_again_after_a_rerun_requeues_its_dependents() {
    // Job 501 needs job 500, and the run is still active because of job 502.
    let jobs = Arc::new(Mutex::new(vec![
        (500, "failure"),
        (501, "failure"),
        (502, "running"),
    ]));
    let state = jobs.clone();
    let stub = serve(move |seen| {
        let mut jobs = state.lock().unwrap();
        match (seen.method.as_str(), seen.path.as_str()) {
            ("GET", "/repos/owner/name/pulls/3") => ok(&pull("abc")),
            ("GET", "/repos/owner/name/actions/runs") => page(
                seen,
                &json!({"workflow_runs": [{"id": 41, "index_in_repo": 7, "status": "running"}]}),
            ),
            ("GET", "/repos/owner/name/actions/runs/41/jobs") => ok(&json!(
                jobs.iter()
                    .map(|(id, status)| json!({"id": id, "status": status}))
                    .collect::<Vec<_>>()
            )),
            ("POST", path) => {
                let id: u64 = path
                    .trim_start_matches("/repos/owner/name/actions/jobs/")
                    .trim_end_matches("/rerun")
                    .parse()
                    .unwrap();
                let Some(job) = jobs.iter().position(|(job, _)| *job == id) else {
                    return rep(404, "");
                };
                if jobs[job].1 != "failure" {
                    return rep(400, r#"{"message":"job is active"}"#);
                }
                for (job, status) in jobs.iter_mut() {
                    if *job >= id && *status == "failure" {
                        *status = "waiting";
                    }
                }
                rep(204, "")
            }
            _ => quiet(seen),
        }
    })
    .await;
    let rerun = stub
        .forge
        .rerun_failed(&account(&stub.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!(rerun, Rerun::Started(vec![41]));
    assert_eq!(
        stub.seen()
            .iter()
            .filter(|seen| seen.method == "POST")
            .map(|seen| seen.path.clone())
            .collect::<Vec<_>>(),
        ["/repos/owner/name/actions/jobs/500/rerun"]
    );
}

#[tokio::test]
async fn rerun_failed_with_no_failed_job_has_nothing_to_rerun() {
    let stub = serve(rerun_stub("success")).await;
    let rerun = stub
        .forge
        .rerun_failed(&account(&stub.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!(rerun, Rerun::NothingToRerun);
    assert!(stub.seen().iter().all(|seen| seen.method == "GET"));
}

#[tokio::test]
async fn rerun_failed_on_a_server_without_the_endpoints_has_nothing_to_rerun() {
    for missing in [
        "/repos/owner/name/actions/runs",
        "/repos/owner/name/actions/jobs/500/rerun",
    ] {
        let inner = rerun_stub("failure");
        let stub = serve(move |seen| {
            if seen.path == missing {
                rep(404, "not found")
            } else {
                inner(seen)
            }
        })
        .await;
        let rerun = stub
            .forge
            .rerun_failed(&account(&stub.forge), REPO, 3)
            .await
            .unwrap();
        assert_eq!(rerun, Rerun::NothingToRerun, "{missing}");
    }
}

#[tokio::test]
async fn rerun_failed_waits_until_the_old_failure_is_gone() {
    let reads = Arc::new(Mutex::new(0));
    let counter = reads.clone();
    let inner = rerun_stub("failure");
    let stub = serve(move |seen| {
        if seen.path == "/repos/owner/name/commits/abc/status" {
            if seen.query.contains("page=2") {
                return page(seen, &json!({"statuses": []}));
            }
            let mut count = counter.lock().unwrap();
            *count += 1;
            let state = if *count < 3 { "failure" } else { "pending" };
            return page(seen, &json!({"statuses": [context("lint", state, "")]}));
        }
        inner(seen)
    })
    .await;
    stub.forge
        .rerun_failed(&account(&stub.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!(*reads.lock().unwrap(), 3);
}

fn report_stub(log: Rep) -> impl Fn(&Seen) -> Rep + Send + Sync {
    move |seen| match seen.path.as_str() {
        "/repos/owner/name/actions/runs" => {
            assert!(seen.query.contains("run_number=7"), "{}", seen.query);
            page(
                seen,
                &json!({"workflow_runs": [{"id": 41, "index_in_repo": 7, "status": "failure"}]}),
            )
        }
        "/repos/owner/name/actions/runs/41/jobs" => ok(&json!([
            {"id": 500, "status": "failure", "html_url": format!("https://{}/owner/name/actions/runs/7/jobs/0/attempt/1", seen.host)},
        ])),
        "/repos/owner/name/actions/jobs/500/logs" => log.clone(),
        _ => rep(404, ""),
    }
}

#[tokio::test]
async fn failure_report_ends_with_the_tail_of_the_job_log() {
    let long = format!("{}END OF LOG", "x".repeat(100_000));
    let stub = serve(report_stub(rep(200, &long))).await;
    let url = format!("{}/owner/name/actions/runs/7/jobs/0", stub.base);
    let report = stub
        .forge
        .failure_report(
            &account(&stub.forge),
            REPO,
            &[job_failed("CI / lint", &url)],
        )
        .await
        .unwrap();
    assert!(
        report.starts_with("Check \"CI / lint\" failed.\n"),
        "{report}"
    );
    assert!(report.trim_end().ends_with("END OF LOG"));
    assert!(report.len() < 31_000, "{}", report.len());
}

#[tokio::test]
async fn failure_report_finds_the_job_under_a_mount_path_or_an_owner_called_actions() {
    for (prefix, repo) in [("/actions", "owner/name"), ("", "actions/name")] {
        let api = format!("/repos/{repo}");
        let stub = stub_at(prefix, move |seen| {
            let jobs_url = format!(
                "https://{}{prefix}/{repo}/actions/runs/7/jobs/0/attempt/1",
                seen.host
            );
            match seen.path.strip_prefix(&api).unwrap_or(&seen.path) {
                "/pulls/3" => ok(&pull("abc")),
                "/commits/abc/status" => page(
                    seen,
                    &json!({"statuses": [context(
                        "lint",
                        "failure",
                        &format!("{repo}/actions/runs/7/jobs/0"),
                    )]}),
                ),
                "/actions/runs" => page(
                    seen,
                    &json!({"workflow_runs": [{"id": 41, "index_in_repo": 7}]}),
                ),
                "/actions/runs/41/jobs" => ok(&json!([{"id": 500, "html_url": jobs_url}])),
                "/actions/jobs/500/logs" => rep(200, "the log"),
                _ => quiet(seen),
            }
        })
        .await;
        let account = account(&stub.forge);
        let Ci::Failed(checks) = stub.forge.status(&account, repo, 3, &[]).await.unwrap().ci else {
            panic!("expected a failed check");
        };
        assert_eq!(
            checks[0].url.as_deref(),
            Some(&*format!("{}/{repo}/actions/runs/7/jobs/0", stub.base))
        );
        let report = stub
            .forge
            .failure_report(&account, repo, &checks)
            .await
            .unwrap();
        assert!(report.contains("the log"), "{prefix} {repo}: {report}");
    }
}

#[tokio::test]
async fn failure_report_notes_a_check_that_is_not_an_actions_job() {
    let stub = serve(report_stub(rep(200, "log"))).await;
    let report = stub
        .forge
        .failure_report(
            &account(&stub.forge),
            REPO,
            &[
                job_failed("woodpecker", "https://ci.example.org/repos/1/pipeline/9"),
                FailedCheck {
                    name: "no link".to_owned(),
                    url: None,
                },
            ],
        )
        .await
        .unwrap();
    assert!(report.contains("Check \"woodpecker\" failed."), "{report}");
    assert!(report.contains("not a Forgejo Actions job"));
    assert!(report.contains("Details: https://ci.example.org/repos/1/pipeline/9"));
    assert!(report.contains("Check \"no link\" failed."));
    assert!(stub.seen().is_empty());
}

#[tokio::test]
async fn failure_report_lists_the_check_when_the_log_cannot_be_read() {
    let stub = serve(report_stub(rep(404, r#"{"message":"no logs"}"#))).await;
    let url = format!("{}/owner/name/actions/runs/7/jobs/0", stub.base);
    let report = stub
        .forge
        .failure_report(
            &account(&stub.forge),
            REPO,
            &[job_failed("CI / lint", &url)],
        )
        .await
        .unwrap();
    assert!(report.contains("Could not fetch its log"), "{report}");
    assert!(report.contains(&format!("Details: {url}")));

    let stub = serve(report_stub(rep(401, "{}"))).await;
    let url = format!("{}/owner/name/actions/runs/7/jobs/0", stub.base);
    let err = stub
        .forge
        .failure_report(
            &account(&stub.forge),
            REPO,
            &[job_failed("CI / lint", &url)],
        )
        .await
        .unwrap_err();
    assert!(err.starts_with(REJECTED), "{err}");
}

#[tokio::test]
async fn merge_squashes_and_deletes_the_branch() {
    let stub = serve(|_| rep(200, "")).await;
    let account = account(&stub.forge);
    stub.forge.merge(&account, REPO, 3).await.unwrap();
    let sent = stub.requests("POST", "/repos/owner/name/pulls/3/merge");
    let body: Value = serde_json::from_str(&sent[0].body).unwrap();
    assert_eq!(
        body,
        json!({"Do": "squash", "delete_branch_after_merge": true})
    );
}

#[tokio::test]
async fn merge_head_binds_the_merge_to_the_head_and_reports_a_refusal() {
    let stub = serve(|_| rep(200, "")).await;
    let account = account(&stub.forge);
    stub.forge
        .merge_head(&account, REPO, 3, "abc")
        .await
        .unwrap();
    let sent = stub.requests("POST", "/repos/owner/name/pulls/3/merge");
    let body: Value = serde_json::from_str(&sent[0].body).unwrap();
    assert_eq!(body["head_commit_id"], "abc");
    assert_eq!(body["Do"], "squash");

    let stale = serve(|_| rep(409, r#"{"message":"head commit id does not match"}"#)).await;
    let err = stale
        .forge
        .merge_head(&account, REPO, 3, "old")
        .await
        .unwrap_err();
    assert!(
        err.contains("HTTP 409") && err.contains("does not match"),
        "{err}"
    );

    let denied = serve(|_| rep(403, r#"{"message":"not allowed"}"#)).await;
    let err = denied.forge.merge(&account, REPO, 3).await.unwrap_err();
    assert!(err.starts_with(NOT_PERMITTED), "{err}");
}

#[tokio::test]
async fn approve_and_comment_post_to_the_pull_request() {
    let stub = serve(|_| rep(200, "{}")).await;
    let account = account(&stub.forge);
    stub.forge
        .approve(&account, REPO, 3, "abc", "looks fine")
        .await
        .unwrap();
    stub.forge
        .comment(&account, REPO, 3, "hello")
        .await
        .unwrap();
    let review = stub.requests("POST", "/repos/owner/name/pulls/3/reviews");
    let body: Value = serde_json::from_str(&review[0].body).unwrap();
    assert_eq!(
        body,
        json!({"event": "APPROVED", "commit_id": "abc", "body": "looks fine"})
    );
    let comment = stub.requests("POST", "/repos/owner/name/issues/3/comments");
    assert_eq!(
        serde_json::from_str::<Value>(&comment[0].body).unwrap(),
        json!({"body": "hello"})
    );
}

#[tokio::test]
async fn open_pr_creates_the_pull_request_or_updates_the_open_one() {
    let description = PrDescription {
        title: "New title".to_owned(),
        body: "New body".to_owned(),
    };
    let created = serve(|seen| match seen.method.as_str() {
        "POST" => rep(201, &pull("abc").to_string()),
        _ => rep(404, ""),
    })
    .await;
    let number = created
        .forge
        .open_pr(
            &account(&created.forge),
            REPO,
            "main",
            "feature",
            &description,
        )
        .await
        .unwrap();
    assert_eq!(number, 3);
    let sent: Value =
        serde_json::from_str(&created.requests("POST", "/repos/owner/name/pulls")[0].body).unwrap();
    assert_eq!(
        sent,
        json!({"head": "feature", "base": "main", "title": "New title", "body": "New body"})
    );

    let existing = serve(|seen| match (seen.method.as_str(), seen.path.as_str()) {
        ("POST", "/repos/owner/name/pulls") => rep(409, r#"{"message":"already exists"}"#),
        ("GET", "/repos/owner/name/pulls") => {
            let mut other = pull("zzz");
            other["number"] = json!(2);
            other["head"]["label"] = json!("other");
            page(seen, &json!([null, other, pull("abc")]))
        }
        ("PATCH", "/repos/owner/name/pulls/3") => rep(201, "{}"),
        _ => rep(404, ""),
    })
    .await;
    let number = existing
        .forge
        .open_pr(
            &account(&existing.forge),
            REPO,
            "main",
            "feature",
            &description,
        )
        .await
        .unwrap();
    assert_eq!(number, 3);
    let patch: Value =
        serde_json::from_str(&existing.requests("PATCH", "/repos/owner/name/pulls/3")[0].body)
            .unwrap();
    assert_eq!(patch, json!({"title": "New title", "body": "New body"}));
}

#[tokio::test]
async fn merged_pr_finds_the_merged_pull_request_of_a_branch() {
    let stub = serve(|seen| {
        let mut closed = pull("a");
        closed["number"] = json!(1);
        closed["state"] = json!("closed");
        let mut merged = pull("b");
        merged["number"] = json!(2);
        merged["state"] = json!("closed");
        merged["merged"] = json!(true);
        page(seen, &json!([null, closed, merged]))
    })
    .await;
    let account = account(&stub.forge);
    assert_eq!(
        stub.forge
            .merged_pr(&account, REPO, "feature")
            .await
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        stub.forge
            .merged_pr(&account, REPO, "nothing")
            .await
            .unwrap(),
        None
    );
    assert!(stub.seen()[0].query.contains("state=closed"));
}

#[tokio::test]
async fn a_pull_request_is_matched_by_branch_label_and_repository_not_by_ref() {
    let stub = serve(|seen| {
        let mut fork = pull("f");
        fork["number"] = json!(1);
        fork["state"] = json!("closed");
        fork["merged"] = json!(true);
        fork["head"]["repo_id"] = json!(9);
        let mut deleted = pull("b");
        deleted["number"] = json!(2);
        deleted["state"] = json!("closed");
        deleted["merged"] = json!(true);
        deleted["head"]["ref"] = json!("refs/pull/2/head");
        page(seen, &json!([fork, deleted]))
    })
    .await;
    let me = account(&stub.forge);
    assert_eq!(
        stub.forge.merged_pr(&me, REPO, "feature").await.unwrap(),
        Some(2)
    );

    let stub = serve(|seen| match (seen.method.as_str(), seen.path.as_str()) {
        ("POST", _) => rep(409, "{}"),
        ("GET", "/repos/owner/name/pulls") => {
            let mut fork = pull("f");
            fork["number"] = json!(1);
            fork["head"]["repo_id"] = json!(9);
            page(seen, &json!([fork]))
        }
        _ => rep(404, ""),
    })
    .await;
    let description = PrDescription {
        title: "t".to_owned(),
        body: "b".to_owned(),
    };
    let err = stub
        .forge
        .open_pr(&account(&stub.forge), REPO, "main", "feature", &description)
        .await
        .unwrap_err();
    assert!(err.contains("not open"), "{err}");
    assert!(
        stub.requests("PATCH", "/repos/owner/name/pulls/1")
            .is_empty()
    );
}

#[tokio::test]
async fn renovate_prs_keeps_the_pull_requests_of_the_author_against_the_base() {
    let stub = serve(|seen| {
        let mine = pull("a");
        let mut other_author = pull("b");
        other_author["number"] = json!(4);
        other_author["user"] = json!({"login": "someone"});
        let mut other_base = pull("c");
        other_base["number"] = json!(5);
        other_base["base"]["ref"] = json!("develop");
        page(seen, &json!([null, mine, other_author, other_base]))
    })
    .await;
    let account = account(&stub.forge);
    let prs = stub
        .forge
        .renovate_prs(&account, REPO, "main", Some("Renovate-Bot"))
        .await
        .unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].number, 3);
    assert_eq!(prs[0].head, "a");
    assert_eq!(prs[0].url, "https://forge.test/owner/name/pulls/3");
    let query = &stub.seen()[0].query;
    assert!(
        query.contains("poster=Renovate-Bot") && query.contains("base=main"),
        "{query}"
    );

    let err = stub
        .forge
        .renovate_prs(&account, REPO, "main", None)
        .await
        .unwrap_err();
    assert!(err.contains("Renovate account"), "{err}");
}

#[tokio::test]
async fn the_rebase_checkbox_is_ticked_only_while_the_head_is_unchanged() {
    let body = "text\n - [ ] <!-- rebase-check -->If you want to rebase\nmore";
    let stub = serve(move |seen| match seen.method.as_str() {
        "GET" => {
            let mut pull = pull("abc");
            pull["body"] = json!(body);
            ok(&pull)
        }
        _ => rep(201, "{}"),
    })
    .await;
    let account = account(&stub.forge);
    let moved = stub
        .forge
        .request_renovate_rebase(&account, REPO, 3, "old")
        .await
        .unwrap();
    assert_eq!(moved, RebaseCheckbox::HeadMoved);
    assert!(
        stub.requests("PATCH", "/repos/owner/name/pulls/3")
            .is_empty()
    );

    let ticked = stub
        .forge
        .request_renovate_rebase(&account, REPO, 3, "abc")
        .await
        .unwrap();
    assert_eq!(ticked, RebaseCheckbox::Ticked);
    let patch: Value =
        serde_json::from_str(&stub.requests("PATCH", "/repos/owner/name/pulls/3")[0].body).unwrap();
    assert_eq!(
        patch["body"],
        "text\n - [x] <!-- rebase-check -->If you want to rebase\nmore"
    );

    let plain = serve(|_| ok(&pull("abc"))).await;
    assert_eq!(
        plain
            .forge
            .request_renovate_rebase(&account, REPO, 3, "abc")
            .await
            .unwrap(),
        RebaseCheckbox::Missing
    );
}

#[tokio::test]
async fn pr_details_reads_the_files_and_the_whole_diff() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/pulls/3/files" => page(
            seen,
            &json!([{"filename": "a.toml"}, {"filename": "b/c.lock"}]),
        ),
        "/repos/owner/name/pulls/3.diff" => rep(200, "diff --git a/a.toml b/a.toml\n"),
        _ => rep(404, ""),
    })
    .await;
    let account = account(&stub.forge);
    let details = stub.forge.pr_details(&account, REPO, 3).await.unwrap();
    assert_eq!(details.title, "Add a thing");
    assert_eq!(details.files, ["a.toml", "b/c.lock"]);
    assert_eq!(details.diff, "diff --git a/a.toml b/a.toml\n");
    let info = stub.forge.pr_info(&account, REPO, 3).await.unwrap();
    assert_eq!(info.head, "abc");
    assert_eq!(info.body, "the body");

    let big = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3.diff" => rep(200, &"d".repeat(200_001)),
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        _ => page(seen, &json!([])),
    })
    .await;
    let err = big.forge.pr_details(&account, REPO, 3).await.unwrap_err();
    assert!(err.contains("larger than"), "{err}");
}

#[tokio::test]
async fn a_head_that_moves_during_the_diff_read_is_read_again() {
    let reads = Arc::new(Mutex::new(0));
    let counter = reads.clone();
    let stub = serve(move |seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => {
            let mut count = counter.lock().unwrap();
            *count += 1;
            ok(&pull(if *count == 1 { "old" } else { "new" }))
        }
        "/repos/owner/name/pulls/3/files" => page(seen, &json!([{"filename": "a"}])),
        "/repos/owner/name/pulls/3.diff" => rep(200, "diff"),
        _ => rep(404, ""),
    })
    .await;
    let details = stub
        .forge
        .pr_details(&account(&stub.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!(details.head, "new");

    let always = Arc::new(Mutex::new(0));
    let counter = always.clone();
    let stub = serve(move |seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => {
            let mut count = counter.lock().unwrap();
            *count += 1;
            ok(&pull(&format!("h{count}")))
        }
        _ => quiet(seen),
    })
    .await;
    let err = stub
        .forge
        .pr_details(&account(&stub.forge), REPO, 3)
        .await
        .unwrap_err();
    assert!(err.contains("kept moving"), "{err}");
}

#[tokio::test]
async fn unresolved_mergeability_is_neither_a_conflict_nor_ready() {
    let cases = [
        (false, false, json!([]), Ci::Pending),
        (
            false,
            false,
            json!([context("a", "success", "")]),
            Ci::Pending,
        ),
        (
            true,
            false,
            json!([context("a", "success", "")]),
            Ci::Pending,
        ),
        (
            true,
            true,
            json!([context("a", "success", "")]),
            Ci::Pending,
        ),
        (
            false,
            false,
            json!([context("a", "pending", "")]),
            Ci::Pending,
        ),
    ];
    for (draft, mergeable, statuses, expected) in cases {
        let stub = serve(move |seen| match seen.path.as_str() {
            "/repos/owner/name/pulls/3" => {
                let mut pull = pull("abc");
                pull["mergeable"] = json!(mergeable);
                pull["draft"] = json!(draft);
                ok(&pull)
            }
            "/repos/owner/name/commits/abc/status" => {
                page(seen, &json!({"statuses": statuses.clone()}))
            }
            _ => quiet(seen),
        })
        .await;
        let status = stub
            .forge
            .status(&account(&stub.forge), REPO, 3, &[])
            .await
            .unwrap();
        assert!(!status.conflicting, "draft {draft}");
        assert_eq!(status.ci, expected, "draft {draft} mergeable {mergeable}");
    }

    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/commits/abc/status" => {
            page(seen, &json!({"statuses": [context("a", "failure", "")]}))
        }
        _ => quiet(seen),
    })
    .await;
    let status = stub
        .forge
        .status(&account(&stub.forge), REPO, 3, &[])
        .await
        .unwrap();
    assert!(matches!(status.ci, Ci::Failed(_)));
}

#[tokio::test]
async fn a_missing_mergeable_with_green_checks_is_not_ready() {
    for absent in [true, false] {
        let stub = serve(move |seen| match seen.path.as_str() {
            "/repos/owner/name/pulls/3" => {
                let mut pull = pull("abc");
                if absent {
                    pull.as_object_mut().unwrap().remove("mergeable");
                } else {
                    pull["mergeable"] = Value::Null;
                }
                ok(&pull)
            }
            "/repos/owner/name/commits/abc/status" => {
                page(seen, &json!({"statuses": [context("a", "success", "")]}))
            }
            _ => quiet(seen),
        })
        .await;
        let status = stub
            .forge
            .status(&account(&stub.forge), REPO, 3, &[])
            .await
            .unwrap();
        assert_eq!(status.ci, Ci::Pending, "absent {absent}");
        assert!(!status.conflicting);
    }
}

#[tokio::test]
async fn rerun_failed_reruns_every_failed_job_of_a_large_run() {
    let stub = serve(|seen| match (seen.method.as_str(), seen.path.as_str()) {
        ("GET", "/repos/owner/name/pulls/3") => ok(&pull("abc")),
        ("GET", "/repos/owner/name/actions/runs") => page(
            seen,
            &json!({"workflow_runs": [{"id": 41, "index_in_repo": 7}]}),
        ),
        ("GET", "/repos/owner/name/actions/runs/41/jobs") => ok(&json!(
            (1..=60)
                .map(|id| json!({"id": id, "status": "failure"}))
                .collect::<Vec<_>>()
        )),
        ("POST", _) => rep(204, ""),
        _ => quiet(seen),
    })
    .await;
    let rerun = stub
        .forge
        .rerun_failed(&account(&stub.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!(rerun, Rerun::Started(vec![41]));
    let posts = stub.seen().iter().filter(|s| s.method == "POST").count();
    assert_eq!(posts, 60);
}

#[tokio::test]
async fn a_listing_that_is_malformed_or_never_ends_is_an_error() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/commits/abc/status" => ok(&json!({"state": "success"})),
        _ => quiet(seen),
    })
    .await;
    let err = stub
        .forge
        .status(&account(&stub.forge), REPO, 3, &[])
        .await
        .unwrap_err();
    assert!(err.contains("holds no list"), "{err}");

    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/pulls/3/files" => ok(&json!({"message": "oops"})),
        _ => quiet(seen),
    })
    .await;
    let err = stub
        .forge
        .pr_info(&account(&stub.forge), REPO, 3)
        .await
        .unwrap_err();
    assert!(err.contains("holds no list"), "{err}");

    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        "/repos/owner/name/pulls/3/files" => ok(&json!([{"filename": "a"}])),
        _ => quiet(seen),
    })
    .await;
    let err = stub
        .forge
        .pr_info(&account(&stub.forge), REPO, 3)
        .await
        .unwrap_err();
    assert!(err.contains("more than"), "{err}");
}

#[tokio::test]
async fn a_malformed_answer_does_not_echo_the_token() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&json!({"number": TOKEN})),
        _ => rep(404, ""),
    })
    .await;
    let account = account(&stub.forge);
    let err = stub.forge.status(&account, REPO, 3, &[]).await.unwrap_err();
    assert!(err.contains("unexpected answer"), "{err}");
    assert!(!err.contains(TOKEN), "{err}");

    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("abc")),
        _ => ok(&json!({"workflow_runs": [{"id": TOKEN}]})),
    })
    .await;
    let err = stub
        .forge
        .rerun_failed(&account, REPO, 3)
        .await
        .unwrap_err();
    assert!(err.contains("unexpected answer"), "{err}");
    assert!(!err.contains(TOKEN), "{err}");
}

#[tokio::test]
async fn head_commit_dates_the_push_that_made_the_head() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("new")),
        "/repos/owner/name/issues/3/timeline" => page(
            seen,
            &json!([
                {"type": "comment", "body": "hi", "created_at": "2026-11-23T08:00:00Z"},
                {"type": "pull_push", "body": "{\"is_force_push\":false,\"commit_ids\":[\"old\"]}", "created_at": "2026-11-23T09:00:00Z"},
                {"type": "pull_push", "body": "{\"is_force_push\":true,\"commit_ids\":[\"old\",\"new\"]}", "created_at": "2026-11-23T20:53:06+01:00"},
            ]),
        ),
        "/repos/owner/name/git/commits/new" => ok(&json!({"created": "2026-11-23T20:00:00+01:00"})),
        _ => rep(404, ""),
    })
    .await;
    let head = stub
        .forge
        .head_commit(&account(&stub.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!(head.sha, "new");
    assert_eq!(head.pushed_at.as_deref(), Some("2026-11-23 19:53:06"));
    assert_eq!(head.committed_at.as_deref(), Some("2026-11-23 19:00:00"));

    let none = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/3" => ok(&pull("new")),
        "/repos/owner/name/git/commits/new" => rep(404, ""),
        _ => page(seen, &json!([])),
    })
    .await;
    let head = none
        .forge
        .head_commit(&account(&none.forge), REPO, 3)
        .await
        .unwrap();
    assert_eq!((head.pushed_at, head.committed_at), (None, None));
}

fn file_stub(
    old: Option<&'static str>,
    new: Option<&'static str>,
) -> impl Fn(&Seen) -> Rep + Send + Sync {
    move |seen| {
        assert_eq!(seen.path, "/repos/owner/name/raw/dir/Cargo.toml");
        let text = if seen.query == "ref=v1" { old } else { new };
        text.map_or_else(|| rep(404, ""), |text| rep(200, text))
    }
}

#[tokio::test]
async fn file_at_reads_the_file_or_none() {
    let stub = serve(file_stub(Some("a\n"), None)).await;
    let account = account(&stub.forge);
    assert_eq!(
        stub.forge
            .file_at(&account, REPO, "dir/Cargo.toml", "v1")
            .await
            .unwrap(),
        Some("a\n".to_owned())
    );
    assert_eq!(
        stub.forge
            .file_at(&account, REPO, "dir/Cargo.toml", "v2")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn compare_patch_is_none_for_an_unchanged_file() {
    let stub = serve(file_stub(Some("a\nb\n"), Some("a\nb\n"))).await;
    let patch = stub
        .forge
        .compare_patch(&account(&stub.forge), REPO, "v1", "v2", "dir/Cargo.toml")
        .await
        .unwrap();
    assert_eq!(patch, None);
    let missing = serve(file_stub(None, None)).await;
    assert_eq!(
        missing
            .forge
            .compare_patch(&account(&missing.forge), REPO, "v1", "v2", "dir/Cargo.toml")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn compare_patch_is_a_unified_diff_of_the_file_at_both_refs() {
    let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n";
    let new = "1\n2\n3\nfour\n5\n6\n7\n8\n9\n10\n11\n12\n13\n";
    let stub = serve(file_stub(Some(old), Some(new))).await;
    let patch = stub
        .forge
        .compare_patch(&account(&stub.forge), REPO, "v1", "v2", "dir/Cargo.toml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        patch,
        "@@ -1,7 +1,7 @@\n 1\n 2\n 3\n-4\n+four\n 5\n 6\n 7\n@@ -10,3 +10,4 @@\n 10\n 11\n 12\n+13"
    );

    let added = serve(file_stub(None, Some("x\ny\n"))).await;
    let patch = added
        .forge
        .compare_patch(&account(&added.forge), REPO, "v1", "v2", "dir/Cargo.toml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(patch, "@@ -0,0 +1,2 @@\n+x\n+y");
    let removed = serve(file_stub(Some("x\n"), None)).await;
    let patch = removed
        .forge
        .compare_patch(&account(&removed.forge), REPO, "v1", "v2", "dir/Cargo.toml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(patch, "@@ -1,1 +0,0 @@\n-x");

    let newline = serve(file_stub(Some("a\nb"), Some("a\nb\n"))).await;
    let patch = newline
        .forge
        .compare_patch(&account(&newline.forge), REPO, "v1", "v2", "dir/Cargo.toml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        patch,
        "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b"
    );
    let crlf = serve(file_stub(Some("a\r\nb\r\n"), Some("a\nb\n"))).await;
    let patch = crlf
        .forge
        .compare_patch(&account(&crlf.forge), REPO, "v1", "v2", "dir/Cargo.toml")
        .await
        .unwrap()
        .unwrap();
    assert!(patch.contains("-a\r\n-b\r\n+a\n+b"), "{patch:?}");
}

#[tokio::test]
async fn compare_range_reads_the_first_compare_link_of_the_upstream() {
    let stub = serve(|_| rep(404, "")).await;
    let base = &stub.base;
    let body = format!(
        "see [diff]({base}/Up/Stream/compare/v1.0.0...v1.1.0) and {base}/Up/Stream/compare/v2...v3, \
         or {base}/other/repo/compare/a...b"
    );
    assert_eq!(
        stub.forge.compare_range(&body, "up/stream"),
        Some(("v1.0.0".to_owned(), "v1.1.0".to_owned()))
    );
    let two_dots = format!("{base}/up/stream/compare/v1..v2 text");
    assert_eq!(
        stub.forge.compare_range(&two_dots, "up/stream"),
        Some(("v1".to_owned(), "v2".to_owned()))
    );
    assert_eq!(stub.forge.compare_range(&body, "nobody/nothing"), None);
    let elsewhere = "https://github.com/up/stream/compare/v1...v2";
    assert_eq!(stub.forge.compare_range(elsewhere, "up/stream"), None);
}

#[test]
fn a_version_parses_with_the_suffixes_forgejo_adds() {
    let version = |text| ForgejoVersion::parse(text);
    let expected = ForgejoVersion {
        major: 17,
        minor: 0,
        patch: 0,
    };
    assert_eq!(version("17.0.0-121-39320c0+gitea-1.22.0"), Ok(expected));
    assert_eq!(version("17.0.0+gitea-1.22.0"), Ok(expected));
    assert_eq!(
        version("12.1.3-0+gitea-1.22.0"),
        Ok(ForgejoVersion {
            major: 12,
            minor: 1,
            patch: 3
        })
    );
    // Gitea answers with no compatibility suffix.
    assert!(version("1.22.0").is_err());
    assert!(version("garbage+gitea-1.22.0").is_err());
    assert!(version("17.0+gitea-1.22.0").is_err());
    assert!(expected.reruns_jobs() && expected.reads_job_logs());
    let sixteen = version("16.2.0+gitea-1.22.0").unwrap();
    assert!(sixteen.reads_job_logs() && !sixteen.reruns_jobs());
    assert!(!version("15.0.0+gitea-1.22.0").unwrap().reads_job_logs());
}

#[tokio::test]
async fn the_version_is_read_from_the_server() {
    let stub = serve(|seen| {
        assert_eq!(seen.path, "/version");
        assert_eq!(seen.auth, None);
        ok(&json!({"version": "17.0.0-121-39320c0+gitea-1.22.0"}))
    })
    .await;
    assert_eq!(
        stub.forge.version().await.unwrap(),
        ForgejoVersion {
            major: 17,
            minor: 0,
            patch: 0
        }
    );
    let client = forgejo_client(&[]).unwrap();
    assert!(forgejo_version(&stub.base, &client).await.is_err());

    for answer in [r#"{"other":1}"#, "not json", r#"{"version":"1.22.0"}"#] {
        let stub = serve(move |_| rep(200, answer)).await;
        assert!(stub.forge.version().await.is_err(), "{answer}");
    }
    let down = serve(|_| rep(500, "")).await;
    assert!(down.forge.version().await.unwrap_err().contains("HTTP 500"));
}

fn numbered(number: u64, head_repo_id: u64) -> Value {
    let mut pull = pull("abc");
    pull["number"] = json!(number);
    pull["head"]["repo_id"] = json!(head_repo_id);
    pull["head"]["ref"] = json!(format!("b{number}"));
    pull
}

#[tokio::test]
async fn an_empty_search_reads_one_page_of_the_latest_open_pull_requests() {
    let stub = serve(|_| {
        let rows: Vec<Value> = (1..=21)
            .map(|n| numbered(n, if n == 2 { 9 } else { 5 }))
            .collect();
        ok(&json!(rows))
    })
    .await;
    let prs = stub
        .forge
        .search_open_prs(&account(&stub.forge), REPO, "")
        .await
        .unwrap();
    assert_eq!(prs.len(), 21);
    assert!(!prs[0].fork && prs[1].fork);
    assert_eq!(prs[1].head_branch, "b2");
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/repos/owner/name/pulls");
    for pair in ["state=open", "sort=recentupdate", "limit=21"] {
        assert!(
            seen[0].query.split('&').any(|p| p == pair),
            "{}",
            seen[0].query
        );
    }
    assert!(!seen[0].query.contains("page="), "{}", seen[0].query);
}

#[tokio::test]
async fn a_title_search_uses_the_issue_search_and_reads_each_row_once() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/issues" => ok(&json!([null, {"number": 4, "title": "Add A Thing"}, {"number": 5, "title": "body only"}, {"number": 6, "title": "a thing too"}])),
        "/repos/owner/name/pulls/4" => {
            let mut draft = numbered(4, 5);
            draft["draft"] = json!(true);
            ok(&draft)
        }
        "/repos/owner/name/pulls/6" => {
            let mut closed = numbered(6, 5);
            closed["state"] = json!("closed");
            ok(&closed)
        }
        _ => rep(404, ""),
    })
    .await;
    let prs = stub
        .forge
        .search_open_prs(&account(&stub.forge), REPO, "a thing")
        .await
        .unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].number, 4);
    assert!(prs[0].draft && !prs[0].fork);
    let issues = stub.requests("GET", "/repos/owner/name/issues");
    assert_eq!(issues.len(), 1);
    for pair in [
        "type=pulls",
        "state=open",
        "limit=21",
        "q=a+thing",
        "sort=recentupdate",
    ] {
        assert!(
            issues[0].query.split('&').any(|p| p == pair),
            "{}",
            issues[0].query
        );
    }
    assert_eq!(stub.seen().len(), 3);
}

#[tokio::test]
async fn a_number_finds_that_pull_request_only_when_it_is_open() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/12" => ok(&numbered(12, 5)),
        "/repos/owner/name/pulls/13" => {
            let mut merged = numbered(13, 5);
            merged["state"] = json!("closed");
            merged["merged"] = json!(true);
            ok(&merged)
        }
        _ => rep(404, ""),
    })
    .await;
    let account = account(&stub.forge);
    for query in ["12", "#12"] {
        let prs = stub
            .forge
            .search_open_prs(&account, REPO, query)
            .await
            .unwrap();
        assert_eq!(prs.len(), 1, "{query}");
        assert_eq!(prs[0].number, 12);
    }
    assert_eq!(
        stub.forge
            .search_open_prs(&account, REPO, "13")
            .await
            .unwrap(),
        Vec::new()
    );
    assert_eq!(
        stub.forge
            .search_open_prs(&account, REPO, "99")
            .await
            .unwrap(),
        Vec::new()
    );
    assert_eq!(stub.seen().len(), 4);
}

#[tokio::test]
async fn pull_request_reads_the_head_repository_and_none_for_a_missing_one() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/pulls/12" => {
            let mut fork = numbered(12, 9);
            fork["draft"] = json!(true);
            fork["head"]["repo"] = json!({"clone_url": "https://forge.test/ana/name.git"});
            ok(&fork)
        }
        _ => rep(404, ""),
    })
    .await;
    let account = account(&stub.forge);
    let pr = stub
        .forge
        .pull_request(&account, REPO, 12)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pr.state, PrState::Open);
    assert!(pr.fork && pr.draft);
    assert_eq!(
        pr.head_repo_clone_url.as_deref(),
        Some("https://forge.test/ana/name.git")
    );
    assert_eq!(
        (pr.head_branch.as_str(), pr.head_sha.as_str()),
        ("b12", "abc")
    );
    assert_eq!(pr.base_branch, "main");
    assert_eq!(pr.body, "the body");
    assert!(
        stub.forge
            .pull_request(&account, REPO, 99)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_listing_with_null_entries_still_returns_the_rest() {
    let stub = serve(|_| ok(&json!([null, numbered(1, 5), null]))).await;
    let prs = stub
        .forge
        .search_open_prs(&account(&stub.forge), REPO, "")
        .await
        .unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(stub.seen().len(), 1);
}

#[tokio::test]
async fn a_title_search_over_a_page_of_body_only_hits_is_an_error_without_a_second_page() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/issues" => {
            let rows: Vec<Value> = (1..=21)
                .map(|n| json!({"number": n, "title": "unrelated"}))
                .collect();
            ok(&json!(rows))
        }
        _ => rep(404, ""),
    })
    .await;
    let err = stub
        .forge
        .search_open_prs(&account(&stub.forge), REPO, "thing")
        .await
        .unwrap_err();
    assert!(err.contains("incomplete"), "{err}");
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].query.contains("page="), "{}", seen[0].query);
}

#[tokio::test]
async fn a_title_search_with_20_title_matches_on_a_full_page_is_an_error() {
    let stub = serve(|seen| match seen.path.as_str() {
        "/repos/owner/name/issues" => {
            let rows: Vec<Value> = (1..=21)
                .map(|n| json!({"number": n, "title": if n == 21 { "unrelated" } else { "a thing" }}))
                .collect();
            ok(&json!(rows))
        }
        path => path
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .map_or_else(|| rep(404, ""), |n| ok(&numbered(n, 5))),
    })
    .await;
    let err = stub
        .forge
        .search_open_prs(&account(&stub.forge), REPO, "thing")
        .await
        .unwrap_err();
    assert!(err.contains("incomplete"), "{err}");
    assert!(stub.seen().iter().all(|seen| !seen.query.contains("page=")));
}
