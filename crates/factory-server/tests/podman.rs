//! Tests for the podman executor's token gate: a run with no resolvable token
//! fails with "no Claude token is set" before it prepares a checkout or starts
//! a container, and a token saved between runs reaches the next run with no
//! restart. The failure keeps the task as its retry target, so the Retry button
//! reruns it; that core behaviour is covered in factory-core's job-flow tests.
//!
//! The store runs in memory and no podman runs: the token gate returns before
//! any container, and the follow-on run fails at the checkout because git
//! points at a missing binary.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::{Agent, Cli, Event, PrWatch};
use factory_runner::{
    Account, Expect, Forge, ForgeKind, Git, GitHub, GitLab, LocalTarget, WorkerConfig,
};
use factory_server::{
    AccountSource, AgentRegistry, AgentRequest, ClaudeToken, CodexLogin, Executor, Forges, GhRepos,
    LoadFuture, LocalKeyOwner, LocalRequest, LocalSecrets, LocalServerProbe, MASTER_KEY_LEN,
    ModelsFuture, Operation, Owner, PlanUsageCache, PodmanExecutor, Rebase, SecretStore, Settings,
    Store,
};

mod support;

/// A subscription `auth.json`, shaped like the one `codex login` writes.
const AUTH: &str = r#"{"tokens":{"access_token":"at-1","refresh_token":"rt-1"},"last_refresh":"2026-11-20T00:00:00Z"}"#;

fn registry(secrets: &Arc<dyn SecretStore>) -> Arc<AgentRegistry> {
    Arc::new(AgentRegistry::new(
        Arc::new(ClaudeToken::new(Arc::clone(secrets), None)),
        Arc::new(CodexLogin::new(Arc::clone(secrets))),
        Arc::clone(secrets),
    ))
}

fn test_account() -> Account {
    Account {
        token: Some("project-token".into()),
        login: "factory-bot".into(),
        name: "factory-bot".into(),
        email: "1+factory-bot@users.noreply.github.com".into(),
    }
}

fn forges(github: GitHub) -> Forges {
    Forges::new(
        Arc::new(github),
        Arc::new(GhRepos {
            gh: PathBuf::from("gh"),
        }),
    )
}

/// A server that has every model loaded, so a Local run goes on to its worker.
struct LoadedModel;

impl LocalServerProbe for LoadedModel {
    fn models<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> ModelsFuture<'a> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn load<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
        _: Duration,
    ) -> LoadFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

fn executor(registry: Arc<AgentRegistry>, data_dir: PathBuf, store: Arc<Store>) -> PodmanExecutor {
    PodmanExecutor {
        base: WorkerConfig::default(),
        // A missing git binary makes the checkout step fail, so a run that gets
        // past the token gate fails there with a different reason.
        git: Git {
            git: PathBuf::from("/nonexistent/git"),
        },
        forges: forges(GitHub::default()),
        data_dir,
        registry,
        store,
        plan_usage: Arc::new(PlanUsageCache::new()),
        local_probe: Arc::new(LoadedModel),
    }
}

fn request() -> AgentRequest {
    AgentRequest {
        job_id: 7,
        forge: ForgeKind::GitHub,
        account: AccountSource::fixed(test_account()),
        repo: "owner/name".into(),
        base: "main".into(),
        branch: None,
        existing: None,
        merge_base: false,
        keep_checkout: false,
        session_id: "0b6f5c1e-8d1a-4c3e-9a57-3c1f2a9d7e10".into(),
        agent_session: None,
        agent: factory_core::DEFAULT_AGENT,
        expect: Expect::Spec,
        prompt: "Write the spec.".into(),
        model: None,
        effort: None,
        mcp: None,
        packages: Vec::new(),
        local: None,
    }
}

fn local_request(cli: Cli) -> AgentRequest {
    AgentRequest {
        agent: Agent::Local,
        model: Some("gemma".into()),
        local: Some(LocalRequest {
            target: LocalTarget {
                cli,
                base_url: "http://gfx1201:9931".into(),
                key: None,
                load_timeout: Duration::from_secs(600),
            },
            key_from: LocalKeyOwner::None,
            setting_cli: cli,
            session: factory_core::Session::Planning,
        }),
        ..request()
    }
}

fn reason(report: factory_runner::RunReport) -> String {
    match report.event {
        Event::AgentFailed { reason } => reason,
        other => panic!("expected AgentFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_run_with_no_token_fails_before_any_container() {
    let store = Arc::new(Store::open_in_memory().expect("open store"));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
    let data_dir = tempfile::tempdir().unwrap();
    let executor = executor(
        registry(&secrets),
        data_dir.path().to_path_buf(),
        Arc::clone(&store),
    );

    let report = executor
        .run_agent(
            &Settings::default(),
            request(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;

    assert_eq!(reason(report), "no Claude token is set");
    // The gate returns before the checkout is prepared, so no job directory and
    // no container were created.
    assert!(!data_dir.path().join("jobs").join("7").exists());
}

#[tokio::test]
async fn a_token_saved_between_runs_reaches_the_next_run() {
    let store = Arc::new(Store::open_in_memory().expect("open store"));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
    let data_dir = tempfile::tempdir().unwrap();
    let executor = executor(
        registry(&secrets),
        data_dir.path().to_path_buf(),
        Arc::clone(&store),
    );

    // No token yet: the run fails at the gate.
    let first = executor
        .run_agent(
            &Settings::default(),
            request(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    assert_eq!(reason(first), "no Claude token is set");

    // Save a token, as the settings page would. The next run resolves it, gets
    // past the gate, and fails later at the checkout instead.
    secrets.set_claude_token("sk-ant-oat01-saved").unwrap();
    let second = executor
        .run_agent(
            &Settings::default(),
            request(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    assert_ne!(reason(second), "no Claude token is set");
}

#[tokio::test]
async fn a_codex_run_with_no_login_fails_before_any_container() {
    let store = Arc::new(Store::open_in_memory().expect("open store"));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
    let data_dir = tempfile::tempdir().unwrap();
    let executor = executor(
        registry(&secrets),
        data_dir.path().to_path_buf(),
        Arc::clone(&store),
    );

    let mut request = request();
    request.agent = factory_core::Agent::Codex;

    // No login yet: the run fails at the gate, before a checkout or container.
    let first = executor
        .run_agent(
            &Settings::default(),
            request.clone(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    assert_eq!(reason(first), "no Codex login is set");
    assert!(!data_dir.path().join("jobs").join("7").exists());

    // Save a login, as the settings page would. The next run resolves it, gets
    // past the gate, and fails later at the checkout instead, so Retry after a
    // login is saved runs the task.
    secrets.set_codex_auth(AUTH).unwrap();
    let second = executor
        .run_agent(
            &Settings::default(),
            request,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    assert_ne!(reason(second), "no Codex login is set");
}

#[tokio::test]
async fn a_copilot_run_with_no_login_fails_before_any_container() {
    let store = Arc::new(Store::open_in_memory().expect("open store"));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
    let data_dir = tempfile::tempdir().unwrap();
    let executor = executor(
        registry(&secrets),
        data_dir.path().to_path_buf(),
        Arc::clone(&store),
    );

    let mut request = request();
    request.agent = factory_core::Agent::Copilot;

    let first = executor
        .run_agent(
            &Settings::default(),
            request.clone(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    assert_eq!(reason(first), "no Copilot login is set");
    assert!(!data_dir.path().join("jobs").join("7").exists());

    secrets.set_copilot_token("gho_saved").unwrap();
    let second = executor
        .run_agent(
            &Settings::default(),
            request,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    assert_ne!(reason(second), "no Copilot login is set");
}

#[tokio::test]
async fn the_real_approval_names_the_checked_head_when_the_pull_request_has_moved() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let gh = dir.path().join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/bash\necho \"$@\" >> '{}'\n\
             case \"$*\" in\n\
             \"api repos/o/r --hostname github.com\") echo '{}';;\n\
             *) echo '{{\"headRefOid\":\"unchecked2\"}}';;\n\
             esac\n",
            calls.display(),
            WRITE_ACCESS
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(
        &[7; MASTER_KEY_LEN],
        Arc::new(Store::open_in_memory().unwrap()),
    ));
    let mut podman = executor(
        registry(&secrets),
        dir.path().to_owned(),
        Arc::new(Store::open_in_memory().unwrap()),
    );
    podman.forges = forges(GitHub {
        gh,
        ..GitHub::default()
    });

    podman
        .merge_renovate(
            &ForgeKind::GitHub,
            &AccountSource::fixed(test_account()),
            "o/r",
            5,
            "checked1",
        )
        .await
        .unwrap();

    let calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(calls.lines().count(), 4, "{calls}");
    assert!(calls.contains("commit_id=checked1"), "{calls}");
    assert!(calls.contains("--match-head-commit checked1"), "{calls}");
    assert!(!calls.contains("unchecked2"), "{calls}");
}

/// A podman executor whose gh answers `pr view` with `view`, a diff that is
/// `diff_bytes` long, a manifest file and a compare. It logs each call.
fn gh_executor(
    dir: &std::path::Path,
    view: &serde_json::Value,
    diff_bytes: usize,
) -> (PodmanExecutor, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let calls = dir.join("calls");
    let view_file = dir.join("view.json");
    std::fs::write(&view_file, view.to_string()).unwrap();
    let gh = dir.join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/bash\necho \"[$GH_TOKEN] $@\" >> '{}'\ncase \"$1 $2\" in\n\
             \"pr view\") cat '{}';;\n\
             \"pr diff\") if [ -f '{diff}' ]; then cat '{diff}'; else head -c {diff_bytes} /dev/zero | tr '\\0' x; fi;;\n\
             \"api --method\") echo '{{\"content\":\"bmFtZQ==\",\"type\":\"file\",\"encoding\":\"base64\"}}';;\n\
             \"api repos/up/app/compare/nope\"*) echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;\n\
             \"api repos/up/app/compare/\"*) echo '{{\"files\":[{{\"filename\":\"CMakeLists.txt\",\"patch\":\"@@ new\"}}]}}';;\n\
             esac\n",
            calls.display(),
            view_file.display(),
            diff = dir.join("diff.txt").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(
        &[7; MASTER_KEY_LEN],
        Arc::new(Store::open_in_memory().unwrap()),
    ));
    let mut podman = executor(
        registry(&secrets),
        dir.to_owned(),
        Arc::new(Store::open_in_memory().unwrap()),
    );
    podman.forges = forges(GitHub {
        gh,
        ..GitHub::default()
    });
    (podman, calls)
}

fn view(head: &str, body: &str, files: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "title": "Update app",
        "body": body,
        "headRefOid": head,
        "files": files.iter().map(|path| serde_json::json!({"path": path})).collect::<Vec<_>>(),
    })
}

fn packaging_request(
    verdict_head: Option<&str>,
    range: Option<(&str, &str)>,
) -> factory_server::PackagingRequest {
    factory_server::PackagingRequest {
        forge: ForgeKind::GitHub,
        account: AccountSource::fixed(test_account()),
        repo: "o/r".into(),
        upstream: "up/app".into(),
        upstream_forge: ForgeKind::GitHub,
        upstream_account: AccountSource::fixed(test_account()),
        base: "main".into(),
        number: 5,
        manifest_files: vec!["snap/snapcraft.yaml".into()],
        build_config_files: vec!["CMakeLists.txt".into()],
        verdict_head: verdict_head.map(str::to_owned),
        from: range.map(|(from, _)| from.to_owned()),
        to: range.map(|(_, to)| to.to_owned()),
    }
}

#[tokio::test]
async fn the_upstream_is_read_with_the_upstream_account_and_the_pull_request_with_the_project_account()
 {
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", COMPARE_BODY, &[]), 10);
    let mut request = packaging_request(None, None);
    request.account = AccountSource::fixed(Account {
        token: Some("ghp_project".into()),
        ..test_account()
    });
    request.upstream_account = AccountSource::fixed(Account {
        token: Some("ghp_upstream".into()),
        ..test_account()
    });

    podman.gather_packaging(request).await.unwrap();

    let calls = std::fs::read_to_string(&calls).unwrap();
    assert!(calls.contains("[ghp_project] pr view"), "{calls}");
    assert!(calls.contains("[ghp_project] api --method"), "{calls}");
    assert!(
        calls.contains("[ghp_upstream] api repos/up/app/compare/v1...v2"),
        "{calls}"
    );
    assert!(!calls.contains("[ghp_project] api repos/up/app"), "{calls}");
}

#[tokio::test]
async fn a_salsa_project_reads_its_merge_request_on_salsa_and_its_github_upstream_on_github() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let script = |name: &str, token: &str, body: &str| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/bash\necho \"{name} [${token}] $@\" >> '{}'\n{body}\n",
                calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let glab = script(
        "glab",
        "GITLAB_TOKEN",
        r#"case "$4" in
  */merge_requests/5/versions/1*) echo '{"state":"collected","head_commit_sha":"h1","real_size":"1","diffs":[{"old_path":"debian/control","new_path":"debian/control","diff":"@@ -1 +1 @@\n-a\n+b\n"}]}';;
  */merge_requests/5/versions*) echo '[{"id":1,"head_commit_sha":"h1","created_at":"2024-01-01T00:00:00Z"}]';;
  */merge_requests/5*) echo '{"iid":5,"title":"Update app","description":"Bumps app. [Compare Source](https://github.com/up/app/compare/v1...v2)","sha":"h1"}';;
  */repository/files/*) echo manifest;;
  *) echo "fake glab: unexpected: $*" >&2; exit 1;;
esac"#,
    );
    let gh = script(
        "gh",
        "GH_TOKEN",
        r#"case "$2" in
  repos/up/app/compare/*) echo '{"files":[{"filename":"CMakeLists.txt","patch":"@@ new"}]}';;
  *) echo "fake gh: unexpected: $*" >&2; exit 1;;
esac"#,
    );
    let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(
        &[7; MASTER_KEY_LEN],
        Arc::new(Store::open_in_memory().unwrap()),
    ));
    let mut podman = executor(
        registry(&secrets),
        dir.path().to_owned(),
        Arc::new(Store::open_in_memory().unwrap()),
    );
    podman.forges = forges(GitHub {
        gh,
        ..GitHub::default()
    })
    .with_salsa(Arc::new(GitLab {
        glab,
        ..GitLab::default()
    }));
    let mut request = packaging_request(None, None);
    request.forge = ForgeKind::Salsa;
    request.repo = "team/pkg".into();
    request.manifest_files = vec!["debian/control".into()];
    request.account = AccountSource::fixed(Account {
        token: Some("glpat-project".into()),
        ..test_account()
    });
    request.upstream_account = AccountSource::fixed(Account {
        token: Some("ghp_upstream".into()),
        ..test_account()
    });

    let context = podman.gather_packaging(request).await.unwrap();

    assert_eq!(context.from.as_deref(), Some("v1"));
    assert_eq!(
        context.manifests[0].text.as_deref().map(str::trim),
        Some("manifest")
    );
    let calls = std::fs::read_to_string(&calls).unwrap();
    for line in calls.lines() {
        if line.starts_with("glab") {
            assert!(line.starts_with("glab [glpat-project] api"), "{line}");
            assert!(line.contains("--hostname salsa.debian.org"), "{line}");
            assert!(!line.contains("up/app"), "{line}");
        } else {
            assert!(
                line.starts_with("gh [ghp_upstream] api repos/up/app/compare/v1...v2"),
                "{line}"
            );
        }
    }
    assert!(
        calls.contains(
            "glab [glpat-project] api --method GET projects/team%2Fpkg/merge_requests/5 "
        )
    );
    assert!(calls.lines().any(|line| line.starts_with("gh ")), "{calls}");
}

/// A Forgejo that serves `up/app` at tags `v1` and `v2`, and pull request 5 of
/// `team/pkg` with `body`. It logs each request's path, query and
/// `Authorization` header.
async fn forgejo_packaging_stub(body: String, seen: Arc<Mutex<Vec<String>>>) -> support::TlsServer {
    use axum::http::{HeaderMap, Uri, header};

    let handler = move |uri: Uri, headers: HeaderMap| {
        let seen = Arc::clone(&seen);
        let body = body.clone();
        async move {
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            seen.lock().unwrap().push(format!(
                "{}?{} | {auth}",
                uri.path(),
                uri.query().unwrap_or("")
            ));
            match (uri.path(), uri.query().unwrap_or("")) {
                ("/api/v1/repos/up/app/raw/CMakeLists.txt", "ref=v1") => "a\n".to_owned(),
                ("/api/v1/repos/up/app/raw/CMakeLists.txt", "ref=v2") => "b\n".to_owned(),
                ("/api/v1/repos/team/pkg/pulls/5", _) => serde_json::json!({
                    "number": 5, "title": "Update app", "body": body,
                    "head": {"sha": "h1"}, "base": {"ref": "main"},
                })
                .to_string(),
                ("/api/v1/repos/team/pkg/pulls/5/files", query) if query.contains("page=1") => {
                    r#"[{"filename":"debian/control"}]"#.to_owned()
                }
                ("/api/v1/repos/team/pkg/pulls/5/files", _) => "[]".to_owned(),
                ("/api/v1/repos/team/pkg/pulls/5.diff", _) => "diff --git a/x b/x\n".to_owned(),
                ("/api/v1/repos/team/pkg/raw/debian/control", _) => "manifest\n".to_owned(),
                _ => String::new(),
            }
        }
    };
    support::serve_tls(axum::Router::new().fallback(handler)).await
}

fn forgejo_kind(stub: &support::TlsServer) -> ForgeKind {
    ForgeKind::Forgejo(factory_runner::ForgejoServer {
        name: "mine".into(),
        base_url: stub.base.clone(),
        registration: 1,
    })
}

fn forgejo_account(stub: &support::TlsServer, token: &str) -> Account {
    factory_runner::Forgejo::new(&stub.base, &[])
        .unwrap()
        .account(
            token.into(),
            &factory_runner::ForgeAccount {
                login: "forge-bot".into(),
                id: 3,
            },
        )
}

fn trust(forges: Forges, stub: &support::TlsServer) -> Forges {
    forges
        .with_forgejo_roots(vec![
            reqwest::Certificate::from_pem(stub.ca_pem.as_bytes()).unwrap(),
        ])
        .unwrap()
}

#[tokio::test]
async fn a_github_project_reads_the_patch_of_a_forgejo_upstream_from_the_compare_link() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stub = forgejo_packaging_stub(String::new(), Arc::clone(&seen)).await;
    let body = format!("Bumps app. [Compare]({}/up/app/compare/v1...v2)", stub.base);
    let (mut podman, calls) = gh_executor(dir.path(), &view("h1", &body, &[]), 10);
    podman.forges = trust(podman.forges, &stub);
    let mut request = packaging_request(None, None);
    request.upstream_forge = forgejo_kind(&stub);
    request.upstream_account = AccountSource::fixed(forgejo_account(&stub, "fj-upstream"));

    let context = podman.gather_packaging(request).await.unwrap();

    assert_eq!(context.from.as_deref(), Some("v1"));
    assert_eq!(context.to.as_deref(), Some("v2"));
    let patch = context.upstream[0].text.as_deref().unwrap();
    assert!(patch.contains("-a") && patch.contains("+b"), "{patch}");
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|call| call.starts_with("/api/v1/repos/up/app/raw/CMakeLists.txt?ref=v1")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .all(|call| call.contains("| Basic ") || call.ends_with("| token fj-upstream")),
        "{seen:?}"
    );
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert!(!calls.contains("up/app/compare"), "{calls}");
}

#[tokio::test]
async fn a_forgejo_project_reads_its_pull_request_on_forgejo_and_its_salsa_upstream_on_salsa() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let glab = dir.path().join("glab");
    std::fs::write(
        &glab,
        format!(
            "#!/bin/bash\necho \"[$GITLAB_TOKEN] $@\" >> '{}'\n\
             case \"$4\" in\n\
             projects/up%2Fapp/repository/compare*) echo '{{\"diffs\":[{{\"new_path\":\"CMakeLists.txt\",\"diff\":\"@@ new\"}}]}}';;\n\
             *) echo \"fake glab: unexpected: $*\" >&2; exit 1;;\nesac\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&glab, std::fs::Permissions::from_mode(0o755)).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let body = "Bumps app. [Compare](https://salsa.debian.org/up/app/-/compare/v1...v2)";
    let stub = forgejo_packaging_stub(body.to_owned(), Arc::clone(&seen)).await;
    let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(
        &[7; MASTER_KEY_LEN],
        Arc::new(Store::open_in_memory().unwrap()),
    ));
    let mut podman = executor(
        registry(&secrets),
        dir.path().to_owned(),
        Arc::new(Store::open_in_memory().unwrap()),
    );
    podman.forges = trust(forges(GitHub::default()), &stub).with_salsa(Arc::new(GitLab {
        glab,
        ..GitLab::default()
    }));
    let mut request = packaging_request(None, None);
    request.forge = forgejo_kind(&stub);
    request.repo = "team/pkg".into();
    request.manifest_files = vec!["debian/control".into()];
    request.account = AccountSource::fixed(forgejo_account(&stub, "fj-project"));
    request.upstream_forge = ForgeKind::Salsa;
    request.upstream_account = AccountSource::fixed(Account {
        token: Some("glpat-upstream".into()),
        ..test_account()
    });

    let context = podman.gather_packaging(request).await.unwrap();

    assert_eq!(context.from.as_deref(), Some("v1"));
    assert_eq!(
        context.manifests[0].text.as_deref().map(str::trim),
        Some("manifest")
    );
    assert_eq!(context.upstream[0].text.as_deref(), Some("@@ new"));
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert!(calls.contains("[glpat-upstream] api"), "{calls}");
    assert!(!calls.contains("fj-project"), "{calls}");
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|call| !call.contains("up/app")), "{seen:?}");
}

const COMPARE_BODY: &str = "Bumps app. [Compare Source](https://github.com/up/app/compare/v1...v2)";

#[tokio::test]
async fn a_merge_that_touches_no_manifest_never_downloads_its_diff() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", "", &["Cargo.lock"]), 300_000);
    let manifests = vec!["snap/snapcraft.yaml".to_owned()];

    let event = podman
        .renovate_merged(
            &ForgeKind::GitHub,
            &AccountSource::fixed(test_account()),
            "o/r",
            5,
            Some(&manifests),
        )
        .await
        .unwrap();

    assert_eq!(
        event,
        Event::RenovateMerged {
            packaging: true,
            manifest_changed: false
        }
    );
    assert!(!std::fs::read_to_string(&calls).unwrap().contains("pr diff"));
}

#[tokio::test]
async fn a_manual_merge_before_any_verdict_takes_the_range_from_the_pull_request() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", COMPARE_BODY, &[]), 10);

    let context = podman
        .gather_packaging(packaging_request(None, None))
        .await
        .unwrap();

    assert_eq!(context.from.as_deref(), Some("v1"));
    assert_eq!(context.to.as_deref(), Some("v2"));
    assert_eq!(context.upstream[0].text.as_deref(), Some("@@ new"));
    assert!(
        std::fs::read_to_string(&calls)
            .unwrap()
            .contains("repos/up/app/compare/v1...v2")
    );
}

const BUMP_DIFF: &str = "diff --git a/snap/snapcraft.yaml b/snap/snapcraft.yaml\n\
--- a/snap/snapcraft.yaml\n+++ b/snap/snapcraft.yaml\n@@ -1,3 +1,3 @@\n name: app\n\
-    base: core22\n+    base: core24\n-    source-tag: b9692\n+    source-tag: b9716\n";

#[tokio::test]
async fn a_manual_merge_without_a_compare_link_takes_the_range_from_the_manifest_diff() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("diff.txt"), BUMP_DIFF).unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", "Bumps app.", &[]), 10);

    let context = podman
        .gather_packaging(packaging_request(None, None))
        .await
        .unwrap();

    assert_eq!(context.from.as_deref(), Some("b9692"));
    assert_eq!(context.to.as_deref(), Some("b9716"));
    assert!(
        std::fs::read_to_string(&calls)
            .unwrap()
            .contains("repos/up/app/compare/b9692...b9716")
    );
}

#[tokio::test]
async fn two_different_version_bumps_in_the_manifest_diff_block_the_analysis() {
    let dir = tempfile::tempdir().unwrap();
    let diff = format!("{BUMP_DIFF}-    source-tag: c1\n+    source-tag: c2\n");
    std::fs::write(dir.path().join("diff.txt"), diff).unwrap();
    let (podman, _) = gh_executor(dir.path(), &view("h1", "Bumps app.", &[]), 10);

    let err = podman
        .gather_packaging(packaging_request(None, None))
        .await
        .unwrap_err();

    assert!(err.contains("blocked"), "{err}");
}

#[tokio::test]
async fn two_removed_versions_for_one_added_version_request_no_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let diff = "diff --git a/snap/snapcraft.yaml b/snap/snapcraft.yaml\n\
--- a/snap/snapcraft.yaml\n+++ b/snap/snapcraft.yaml\n@@ -1,3 +1,3 @@\n\
-    source-tag: b9692\n-    source-tag: c1\n+    source-tag: b9716\n";
    std::fs::write(dir.path().join("diff.txt"), diff).unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", "Bumps app.", &[]), 10);

    let err = podman
        .gather_packaging(packaging_request(None, None))
        .await
        .unwrap_err();

    assert!(err.contains("blocked"), "{err}");
    assert!(!std::fs::read_to_string(&calls).unwrap().contains("compare"));
}

#[tokio::test]
async fn a_removal_and_an_addition_in_different_manifests_request_no_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let diff = "diff --git a/snap/snapcraft.yaml b/snap/snapcraft.yaml\n\
--- a/snap/snapcraft.yaml\n+++ b/snap/snapcraft.yaml\n@@ -1 +1 @@\n\
-    source-tag: b9692\n\
diff --git a/other/snapcraft.yaml b/other/snapcraft.yaml\n\
--- a/other/snapcraft.yaml\n+++ b/other/snapcraft.yaml\n@@ -1 +1 @@\n\
+    source-tag: b9716\n";
    std::fs::write(dir.path().join("diff.txt"), diff).unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", "Bumps app.", &[]), 10);
    let mut request = packaging_request(None, None);
    request.manifest_files.push("other/snapcraft.yaml".into());

    let err = podman.gather_packaging(request).await.unwrap_err();

    assert!(err.contains("blocked"), "{err}");
    assert!(!std::fs::read_to_string(&calls).unwrap().contains("compare"));
}

#[tokio::test]
async fn a_removal_and_an_addition_in_separate_hunks_request_no_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let diff = "diff --git a/snap/snapcraft.yaml b/snap/snapcraft.yaml\n\
--- a/snap/snapcraft.yaml\n+++ b/snap/snapcraft.yaml\n@@ -1,2 +1,1 @@\n\
 name: app\n-    source-tag: b9692\n\
@@ -20,1 +19,2 @@\n part: two\n+    source-tag: b9716\n";
    std::fs::write(dir.path().join("diff.txt"), diff).unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", "Bumps app.", &[]), 10);

    let err = podman
        .gather_packaging(packaging_request(None, None))
        .await
        .unwrap_err();

    assert!(err.contains("blocked"), "{err}");
    assert!(!std::fs::read_to_string(&calls).unwrap().contains("compare"));
}

#[tokio::test]
async fn a_comparison_github_rejects_blocks_the_analysis() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, _) = gh_executor(
        dir.path(),
        &view(
            "h1",
            "[c](https://github.com/up/app/compare/nope...gone)",
            &[],
        ),
        10,
    );

    let err = podman
        .gather_packaging(packaging_request(None, None))
        .await
        .unwrap_err();

    assert!(err.contains("not a valid comparison"), "{err}");
}

#[tokio::test]
async fn an_unresolvable_range_with_build_config_files_is_reported_as_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, _) = gh_executor(dir.path(), &view("h1", "Bumps app.", &[]), 10);

    let err = podman
        .gather_packaging(packaging_request(None, None))
        .await
        .unwrap_err();

    assert!(err.contains("blocked"), "{err}");
}

#[tokio::test]
async fn a_verdict_for_an_older_head_is_not_used_for_the_merged_head() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("new", COMPARE_BODY, &[]), 10);

    let context = podman
        .gather_packaging(packaging_request(Some("old"), Some(("v0", "v0b"))))
        .await
        .unwrap();

    assert_eq!(context.from.as_deref(), Some("v1"));
    assert_eq!(context.to.as_deref(), Some("v2"));
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert!(!calls.contains("v0"), "{calls}");
}

#[tokio::test]
async fn a_verdict_for_the_merged_head_gives_its_range() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, _) = gh_executor(dir.path(), &view("new", "Bumps app.", &[]), 10);

    let context = podman
        .gather_packaging(packaging_request(Some("new"), Some(("va", "vb"))))
        .await
        .unwrap();

    assert_eq!(context.from.as_deref(), Some("va"));
    assert_eq!(context.to.as_deref(), Some("vb"));
}

#[tokio::test]
async fn a_rebase_request_ticks_the_checkbox_only_while_the_head_is_the_one_that_merged() {
    let body = "Bumps app\r\n\r\n - [ ] <!-- rebase-check -->rebase\r\n";
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", body, &[]), 0);
    let account = AccountSource::fixed(account_with_token("bot", 1, "ghp_bot"));

    let moved = podman
        .request_rebase(&ForgeKind::GitHub, &account, "o/r", 5, "h0")
        .await;
    assert_eq!(moved, Ok(Rebase::HeadMoved));
    assert!(!std::fs::read_to_string(&calls).unwrap().contains("PATCH"));

    let requested = podman
        .request_rebase(&ForgeKind::GitHub, &account, "o/r", 5, "h1")
        .await;
    assert_eq!(requested, Ok(Rebase::Requested));
    assert!(std::fs::read_to_string(&calls).unwrap().contains("PATCH"));

    let dir = tempfile::tempdir().unwrap();
    let (podman, _) = gh_executor(dir.path(), &view("h1", "no checkbox", &[]), 0);
    let missing = podman
        .request_rebase(&ForgeKind::GitHub, &account, "o/r", 5, "h1")
        .await;
    assert_eq!(missing, Ok(Rebase::NoCheckbox));
}

fn account_with_token(login: &str, id: u64, token: &str) -> Account {
    Account {
        token: Some(token.into()),
        login: login.into(),
        name: login.into(),
        email: format!("{id}+{login}@users.noreply.github.com"),
    }
}

/// Every call a fake `git` or `gh` got: its arguments and the token it was
/// handed, which is `GH_TOKEN` for gh and the `GIT_CONFIG_VALUE_0` header for
/// git.
struct Tools {
    dir: tempfile::TempDir,
}

impl Tools {
    fn new() -> Tools {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        support::executable(
            dir.path(),
            "gh",
            &format!(
                "#!/bin/bash\necho \"gh $* | ${{GH_TOKEN}}\" >> '{calls}'\n\
                 case \"$*\" in\n\
                 *--state\\ merged*) ;;\n\
                 *--state\\ open*) ;;\n\
                 *\"pr create\"*) echo https://github.com/o/r/pull/9 ;;\n\
                 esac\n",
                calls = calls.display()
            ),
        );
        support::executable(
            dir.path(),
            "git",
            &format!(
                "#!/bin/bash\necho \"git $* | ${{GIT_CONFIG_VALUE_0}}\" >> '{calls}'\n\
                 while [ \"$1\" = -c ]; do shift 2; done\n\
                 case \"$1\" in --git-dir=*) shift ;; esac\n\
                 case \"$1\" in\n\
                 clone) mkdir -p \"${{@: -1}}/.git\" ;;\n\
                 log) echo 'abc1234 one' ;;\n\
                 config) case \"$*\" in *--get-regexp*) exit 1 ;; esac ;;\n\
                 esac\n",
                calls = calls.display()
            ),
        );
        Tools { dir }
    }

    fn executor(&self, store: Arc<Store>) -> PodmanExecutor {
        let secrets: Arc<dyn SecretStore> =
            Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
        secrets.set_claude_token("sk-ant-oat01-saved").unwrap();
        let mut podman = executor(registry(&secrets), self.dir.path().join("data"), store);
        podman.git = Git {
            git: self.dir.path().join("git"),
        };
        podman.forges = forges(GitHub {
            gh: self.dir.path().join("gh"),
            ..GitHub::default()
        });
        podman
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }
}

fn header(token: &str) -> String {
    GitHub::default()
        .git_auth_header(&account_with_token("any", 1, token))
        .unwrap()
}

#[tokio::test]
async fn clone_push_and_every_gh_call_use_the_token_of_the_account_the_request_names() {
    let tools = Tools::new();
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let mine = account_with_token("alice-bot", 5, "ghp_requester");
    let mut run = request();
    run.account = AccountSource::fixed(mine.clone());
    run.branch = Some("factory/job-7-pr-1".into());

    // The runs fail at `podman`, after the checkout they prepared. The second
    // finds the checkout and fetches into it.
    for _ in 0..2 {
        podman
            .run_agent(
                &Settings::default(),
                run.clone(),
                Box::new(|_| {}),
                Box::new(|_| {}),
            )
            .await;
    }
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work")).unwrap();
    podman
        .publish(factory_server::PublishRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: AccountSource::fixed(mine),
            repo: "o/r".into(),
            base: "main".into(),
            branch: "factory/job-7-pr-1".into(),
            existing: None,
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
            checks: false,
            keep_history: None,
            pushed: factory_server::PushLog::new(|_| {}),
        })
        .await
        .unwrap();

    let calls = tools.calls();
    let git: Vec<&str> = calls.lines().filter(|c| c.starts_with("git ")).collect();
    let gh: Vec<&str> = calls.lines().filter(|c| c.starts_with("gh ")).collect();
    let expected = header("ghp_requester");
    for verb in ["clone", "fetch", "push"] {
        let call = git
            .iter()
            .find(|c| c.contains(&format!(" {verb} ")))
            .unwrap_or_else(|| panic!("no git {verb} in {calls}"));
        assert!(call.ends_with(&format!("| {expected}")), "{call}");
    }
    assert!(gh.len() >= 3, "{calls}");
    for call in &gh {
        assert!(call.ends_with("| ghp_requester"), "{call}");
    }
    // The token is in the environment only, never in an argument.
    for call in calls.lines() {
        let args = call.rsplit_once(" | ").unwrap().0;
        assert!(!args.contains("ghp_requester"), "{call}");
    }
}

#[tokio::test]
async fn each_call_uses_the_account_of_its_own_request() {
    let tools = Tools::new();
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    for token in ["ghp_first", "ghp_second"] {
        podman
            .forges
            .forge(&ForgeKind::GitHub)
            .merged_pr(&account_with_token("bot", 1, token), "o/r", "b")
            .await
            .unwrap();
    }

    let calls = tools.calls();
    let tokens: Vec<&str> = calls
        .lines()
        .map(|call| call.rsplit_once("| ").unwrap().1)
        .collect();
    assert_eq!(tokens, ["ghp_first", "ghp_second"]);
}

#[tokio::test]
async fn a_token_that_changes_during_a_publish_applies_to_the_next_operation() {
    let tools = Tools::new();
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work")).unwrap();
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let source = AccountSource::new(move || {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        Ok(Operation {
            forge: ForgeKind::GitHub,
            account: account_with_token(&format!("bot{n}"), n as u64, &format!("ghp_token{n}")),
            owner: Owner::Project {
                project: "o".into(),
            },
            ignore: Vec::new(),
        })
    });
    let pushes = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&pushes);
    let published = podman
        .publish(factory_server::PublishRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: source,
            repo: "o/r".into(),
            base: "main".into(),
            branch: "factory/job-7-pr-1".into(),
            existing: None,
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
            checks: false,
            keep_history: None,
            pushed: factory_server::PushLog::new(move |account| {
                log.lock().unwrap().push(account.to_owned());
            }),
        })
        .await
        .unwrap();

    let calls = tools.calls();
    let token_of = |needle: &str| -> String {
        let call = calls
            .lines()
            .find(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("no {needle} in {calls}"));
        call.rsplit_once(" | ").unwrap().1.to_owned()
    };
    assert_eq!(token_of("gh pr list"), "ghp_token1");
    assert_eq!(token_of(" push "), header("ghp_token2"));
    assert_eq!(token_of("gh pr create"), "ghp_token3");
    assert_eq!(published, factory_server::Published::Open(9));
    assert_eq!(*pushes.lock().unwrap(), ["bot2 (project account)"]);
}

#[tokio::test]
async fn a_push_is_logged_even_when_the_pull_request_step_has_no_account() {
    let tools = Tools::new();
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work")).unwrap();
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = AccountSource::new(move || {
        let n = asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if n > 2 {
            return Err(factory_server::ResolveError::Account(
                "the token was cleared".into(),
            ));
        }
        Ok(Operation {
            forge: ForgeKind::GitHub,
            account: account_with_token(&format!("bot{n}"), n as u64, &format!("ghp_token{n}")),
            owner: Owner::Project {
                project: "o".into(),
            },
            ignore: Vec::new(),
        })
    });
    let pushes = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&pushes);
    let err = podman
        .publish(factory_server::PublishRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: source,
            repo: "o/r".into(),
            base: "main".into(),
            branch: "factory/job-7-pr-1".into(),
            existing: None,
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
            checks: false,
            keep_history: None,
            pushed: factory_server::PushLog::new(move |account| {
                log.lock().unwrap().push(account.to_owned());
            }),
        })
        .await
        .unwrap_err();
    assert!(err.starts_with("credentials: "), "{err}");
    assert_eq!(*pushes.lock().unwrap(), ["bot2 (project account)"]);
}

fn rotating_source() -> AccountSource {
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    AccountSource::new(move || {
        let n = asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        Ok(Operation {
            forge: ForgeKind::GitHub,
            account: account_with_token("bot", 1, &format!("ghp_token{n}")),
            owner: Owner::Project {
                project: "o".into(),
            },
            ignore: Vec::new(),
        })
    })
}

#[tokio::test]
async fn a_token_that_changes_after_the_clone_reaches_the_fetch() {
    let tools = Tools::new();
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let mut run = request();
    run.account = rotating_source();
    run.branch = Some("factory/job-7-pr-1".into());

    podman
        .run_agent(
            &Settings::default(),
            run,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;

    let calls = tools.calls();
    let token_of = |needle: &str| -> String {
        let call = calls
            .lines()
            .find(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("no {needle} in {calls}"));
        call.rsplit_once(" | ").unwrap().1.to_owned()
    };
    assert_eq!(token_of(" clone "), header("ghp_token1"));
    assert_eq!(token_of("--git-dir"), header("ghp_token2"));
}

#[tokio::test]
async fn a_refused_token_reading_a_ci_log_after_a_good_status_read_fails_the_watch() {
    let tools = Tools::new();
    support::executable(
        tools.dir.path(),
        "gh",
        "#!/bin/bash\n\
         case \"$*\" in\n\
         *\"run view\"*) echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1 ;;\n\
         *\"pr view\"*) echo '{\"state\":\"OPEN\",\"headRefOid\":\"h1\",\"statusCheckRollup\":[{\"name\":\"Lint\",\"status\":\"COMPLETED\",\"conclusion\":\"FAILURE\",\"detailsUrl\":\"https://github.com/o/r/actions/runs/4/job/5\"}]}' ;;\n\
         esac\n",
    );
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let source = AccountSource::new(|| {
        Ok(Operation {
            forge: ForgeKind::GitHub,
            account: account_with_token("bot", 1, "ghp_token"),
            owner: Owner::Requester {
                login: "alice".into(),
                forge: ForgeKind::GitHub,
            },
            ignore: Vec::new(),
        })
    });
    let err = podman
        .watch(factory_server::WatchRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: source,
            repo: "o/r".into(),
            base: "main".into(),
            number: 5,
            seen_until: None,
            auto_merge: false,
            may_merge: true,
            llm_review: false,
            review_approved: false,
            watch: PrWatch::Full,
        })
        .await
        .unwrap_err();
    assert_eq!(
        factory_server::credential_reason(&err),
        Some(
            "The forge rejected the GitHub token of alice. Save a new one on /settings/forges/github"
        )
    );
}

#[tokio::test]
async fn a_refused_token_names_its_owner_where_the_command_ran() {
    let tools = Tools::new();
    support::executable(
        tools.dir.path(),
        "gh",
        "#!/bin/bash\necho 'gh: Bad credentials (HTTP 401)' >&2\nexit 1\n",
    );
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let source = AccountSource::new(|| {
        Ok(Operation {
            forge: ForgeKind::GitHub,
            account: account_with_token("bot", 1, "ghp_token"),
            owner: Owner::Requester {
                login: "alice".into(),
                forge: ForgeKind::GitHub,
            },
            ignore: Vec::new(),
        })
    });
    let err = podman
        .comment(&ForgeKind::GitHub, &source, "o/r", 5, "hi")
        .await
        .unwrap_err();
    assert_eq!(
        factory_server::credential_reason(&err),
        Some(
            "The forge rejected the GitHub token of alice. Save a new one on /settings/forges/github"
        )
    );
    assert!(!err.contains("ghp_token"), "{err}");
}

const READ_ACCESS: &str =
    r#"{"permissions":{"admin":false,"maintain":false,"push":false,"pull":true}}"#;
const WRITE_ACCESS: &str =
    r#"{"permissions":{"admin":false,"maintain":false,"push":true,"pull":true}}"#;

fn renovate_watch(auto_merge: bool) -> factory_server::RenovateWatchRequest {
    factory_server::RenovateWatchRequest {
        job_id: 1,
        forge: ForgeKind::GitHub,
        account: AccountSource::fixed(test_account()),
        repo: "o/r".into(),
        number: 5,
        verdict_head: Some("h1".into()),
        known_head: None,
        watch_only: false,
        limits: factory_core::RetryLimits { auto: 3, total: 7 },
        auto_merge,
        manifest_files: None,
    }
}

/// A gh whose pull request is open on `head` with failed CI, and whose
/// repository permissions come from the `access.json` it reads on each call.
fn access_executor(dir: &std::path::Path, head: &str) -> (PodmanExecutor, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let calls = dir.join("calls");
    std::fs::write(dir.join("access.json"), READ_ACCESS).unwrap();
    std::fs::write(
        dir.join("view.json"),
        serde_json::json!({
            "state": "OPEN",
            "headRefOid": head,
            "statusCheckRollup": [{"name": "build", "status": "COMPLETED", "conclusion": "FAILURE"}],
        })
        .to_string(),
    )
    .unwrap();
    let gh = dir.join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/bash\necho \"$@\" >> '{calls}'\ncase \"$*\" in\n\
             \"pr view\"*) cat '{view}';;\n\
             \"api --paginate\"*) ;;\n\
             \"api repos/o/r --hostname github.com\") cat '{access}';;\n\
             esac\n",
            calls = calls.display(),
            view = dir.join("view.json").display(),
            access = dir.join("access.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(
        &[7; MASTER_KEY_LEN],
        Arc::new(Store::open_in_memory().unwrap()),
    ));
    let mut podman = executor(
        registry(&secrets),
        dir.to_owned(),
        Arc::new(Store::open_in_memory().unwrap()),
    );
    podman.forges = forges(GitHub {
        gh,
        ..GitHub::default()
    });
    (podman, calls)
}

fn observed_access(event: Option<Event>) -> factory_core::RenovateAccess {
    let Some(Event::RenovateObserved { access, .. }) = event else {
        panic!("expected an observation, got {event:?}");
    };
    *access
}

#[tokio::test]
async fn a_watch_reports_read_access_and_a_later_watch_reports_the_granted_write() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = access_executor(dir.path(), "h1");

    let access = observed_access(podman.watch_renovate(renovate_watch(true)).await.unwrap());
    assert!(!access.can_rerun && !access.can_merge, "{access:?}");
    assert_eq!(access.account, "factory-bot");

    std::fs::write(dir.path().join("access.json"), WRITE_ACCESS).unwrap();
    let access = observed_access(podman.watch_renovate(renovate_watch(true)).await.unwrap());
    assert!(access.can_rerun && access.can_merge, "{access:?}");

    let calls = std::fs::read_to_string(&calls).unwrap();
    for forbidden in ["run rerun", "--approve", "pr merge", "/reviews"] {
        assert!(
            !calls.lines().any(|call| call.contains(forbidden)),
            "{calls}"
        );
    }
}

#[tokio::test]
async fn a_watch_fails_when_the_access_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, _) = access_executor(dir.path(), "h1");
    std::fs::write(dir.path().join("access.json"), "not json").unwrap();

    assert!(podman.watch_renovate(renovate_watch(false)).await.is_err());
}

fn wrote(calls: &str) -> bool {
    calls.lines().any(|call| {
        ["run rerun", "--approve", "pr merge", "/reviews"]
            .iter()
            .any(|write| call.contains(write))
    })
}

#[tokio::test]
async fn a_rerun_and_a_merge_check_the_access_of_the_account_they_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = access_executor(dir.path(), "h1");
    let account = AccountSource::fixed(test_account());

    let rerun = podman
        .rerun(factory_server::RerunRequest {
            job_id: 1,
            forge: ForgeKind::GitHub,
            account: account.clone(),
            repo: "o/r".into(),
            number: 5,
            renovate: true,
        })
        .await;
    assert!(
        rerun
            .unwrap_err()
            .starts_with(factory_runner::NOT_PERMITTED)
    );
    let merge = podman
        .merge_renovate(&ForgeKind::GitHub, &account, "o/r", 5, "h1")
        .await;
    assert!(
        merge
            .unwrap_err()
            .starts_with(factory_runner::NOT_PERMITTED)
    );
    assert!(!wrote(&std::fs::read_to_string(&calls).unwrap()));

    std::fs::write(dir.path().join("access.json"), WRITE_ACCESS).unwrap();
    let rerun = podman
        .rerun(factory_server::RerunRequest {
            job_id: 1,
            forge: ForgeKind::GitHub,
            account: account.clone(),
            repo: "o/r".into(),
            number: 5,
            renovate: true,
        })
        .await;
    if let Err(err) = rerun {
        assert!(!err.starts_with(factory_runner::NOT_PERMITTED), "{err}");
    }
    let merge = podman
        .merge_renovate(&ForgeKind::GitHub, &account, "o/r", 5, "h1")
        .await;
    if let Err(err) = merge {
        assert!(!err.starts_with(factory_runner::NOT_PERMITTED), "{err}");
    }
}

#[tokio::test]
async fn a_push_between_the_status_and_the_diff_polls_again() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let (podman, _) = access_executor(dir.path(), "h2");
    let view = |head: &str| {
        serde_json::json!({
            "state": "OPEN",
            "title": "Update app",
            "body": "",
            "headRefOid": head,
            "files": [{"path": "a"}],
            "statusCheckRollup": [{"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        })
        .to_string()
    };
    std::fs::write(dir.path().join("first.json"), view("h2")).unwrap();
    std::fs::write(dir.path().join("later.json"), view("h3")).unwrap();
    let gh = dir.path().join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/bash\ncase \"$*\" in\n\
             \"pr view\"*) if [ -f '{n}' ]; then cat '{later}'; else touch '{n}'; cat '{first}'; fi;;\n\
             \"pr diff\"*) echo 'diff';;\n\
             \"api repos/o/r --hostname github.com\") echo '{WRITE_ACCESS}';;\n\
             esac\n",
            n = dir.path().join("seen").display(),
            first = dir.path().join("first.json").display(),
            later = dir.path().join("later.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let event = podman.watch_renovate(renovate_watch(false)).await.unwrap();
    assert_eq!(event, None);
}

/// A Forgejo that lists no closed pull request and creates number 9. It logs
/// the method, path and `Authorization` header of each request.
async fn forgejo_stub(seen: Arc<Mutex<Vec<String>>>) -> support::TlsServer {
    use axum::http::{HeaderMap, Method, Uri, header};

    let handler = move |method: Method, uri: Uri, headers: HeaderMap| {
        let seen = Arc::clone(&seen);
        async move {
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            seen.lock()
                .unwrap()
                .push(format!("{method} {} | {auth}", uri.path()));
            if method == Method::POST {
                r#"{"number":9}"#.to_owned()
            } else {
                "[]".to_owned()
            }
        }
    };
    support::serve_tls(axum::Router::new().fallback(handler)).await
}

#[tokio::test]
async fn a_forgejo_request_clones_pushes_and_opens_with_the_token_of_its_account() {
    let tools = Tools::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stub = forgejo_stub(Arc::clone(&seen)).await;
    let kind = ForgeKind::Forgejo(factory_runner::ForgejoServer {
        name: "mine".into(),
        base_url: stub.base.clone(),
        registration: 1,
    });
    let mut podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    podman.forges = podman
        .forges
        .with_forgejo_roots(vec![
            reqwest::Certificate::from_pem(stub.ca_pem.as_bytes()).unwrap(),
        ])
        .unwrap();
    let forgejo = factory_runner::Forgejo::new(&stub.base, &[]).unwrap();
    let mine = forgejo.account(
        "fj-token".into(),
        &factory_runner::ForgeAccount {
            login: "forge-bot".into(),
            id: 3,
        },
    );
    let expected = forgejo.git_auth_header(&mine).unwrap();
    assert_eq!(mine.email, "forge-bot@noreply.127.0.0.1");

    let mut run = request();
    run.forge = kind.clone();
    run.account = AccountSource::fixed(mine.clone());
    run.repo = "team/forged".into();
    run.branch = Some("factory/job-7-pr-1".into());
    podman
        .run_agent(
            &Settings::default(),
            run,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work")).unwrap();
    let published = podman
        .publish(factory_server::PublishRequest {
            job_id: 7,
            forge: kind,
            account: AccountSource::fixed(mine),
            repo: "team/forged".into(),
            base: "main".into(),
            branch: "factory/job-7-pr-1".into(),
            existing: None,
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
            checks: false,
            keep_history: None,
            pushed: factory_server::PushLog::new(|_| {}),
        })
        .await
        .unwrap();
    assert!(matches!(published, factory_server::Published::Open(9)));

    let calls = tools.calls();
    for verb in ["clone", "push"] {
        let call = calls
            .lines()
            .find(|call| call.starts_with("git ") && call.contains(&format!(" {verb} ")))
            .unwrap_or_else(|| panic!("no git {verb} in {calls}"));
        assert!(call.ends_with(&format!("| {expected}")), "{call}");
    }
    assert!(
        calls.contains(&format!("{}/team/forged.git", stub.base)),
        "{calls}"
    );
    assert!(!calls.contains("fj-token"), "{calls}");
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|call| call == "POST /api/v1/repos/team/forged/pulls | token fj-token"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_local_run_needs_no_subscription_login_and_keeps_its_own_config_directory() {
    for (cli, local_dir, own_dir) in [
        (Cli::ClaudeCode, "local-claude", "claude"),
        (Cli::Codex, "local-codex", "codex"),
        (Cli::Copilot, "local-copilot", "copilot"),
    ] {
        let store = Arc::new(Store::open_in_memory().expect("open store"));
        let secrets: Arc<dyn SecretStore> =
            Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
        let data_dir = tempfile::tempdir().unwrap();
        let executor = executor(
            registry(&secrets),
            data_dir.path().to_path_buf(),
            Arc::clone(&store),
        );

        // No Claude token, Codex login or Copilot token is stored. The run still
        // gets past the credential gate and fails at the checkout, since git is
        // a missing binary here, before any container starts.
        let report = executor
            .run_agent(
                &Settings::default(),
                local_request(cli),
                Box::new(|_| {}),
                Box::new(|_| {}),
            )
            .await;

        assert!(
            reason(report).starts_with("could not prepare the checkout"),
            "{cli:?}"
        );
        let job_dir = data_dir.path().join("jobs").join("7");
        assert!(job_dir.join(local_dir).is_dir(), "{cli:?}");
        assert!(!job_dir.join(own_dir).exists(), "{cli:?}");
    }
}

async fn local_launch(
    podman_path: &str,
    cli: Cli,
) -> (Option<Cli>, factory_runner::RunReport, Arc<Store>) {
    let tools = Tools::new();
    let store = Arc::new(Store::open_in_memory().expect("open store"));
    let mut podman = tools.executor(Arc::clone(&store));
    podman.base.podman = if podman_path.starts_with('/') {
        PathBuf::from(podman_path)
    } else {
        support::executable(tools.dir.path(), "podman", podman_path)
    };
    let mut run = local_request(cli);
    run.branch = Some("factory/job-7-pr-1".into());
    let id = store.create("maestro", "alice", "x").unwrap();
    run.job_id = id;
    store
        .session_uuid(id, factory_core::Session::Planning, Agent::Local)
        .unwrap();
    let report = podman
        .run_agent(
            &Settings::default(),
            run,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
    let recorded = store
        .session_local_cli(id, factory_core::Session::Planning)
        .unwrap();
    (recorded, report, store)
}

#[tokio::test]
async fn a_local_session_records_no_cli_when_podman_cannot_start() {
    let (recorded, report, _) = local_launch("/nonexistent/podman", Cli::Codex).await;

    assert!(reason(report).starts_with("could not start"));
    assert_eq!(recorded, None);
}

#[tokio::test]
async fn a_local_session_records_no_cli_when_podman_cannot_start_the_container() {
    let (recorded, _, _) = local_launch(
        "#!/bin/bash\necho 'image not found' >&2\nexit 125\n",
        Cli::Copilot,
    )
    .await;

    assert_eq!(recorded, None);
}

#[tokio::test]
async fn a_local_session_records_its_cli_once_the_worker_has_started() {
    let (recorded, _, _) = local_launch("#!/bin/bash\necho '{}'\nexit 0\n", Cli::Copilot).await;

    assert_eq!(recorded, Some(Cli::Copilot));
}

#[tokio::test]
async fn publishing_to_a_picked_pull_request_pushes_its_branch_and_never_opens_or_edits_one() {
    let tools = Tools::new();
    let calls = tools.dir.path().join("calls");
    support::executable(
        tools.dir.path(),
        "gh",
        &format!(
            "#!/bin/bash\necho \"gh $* | ${{GH_TOKEN}}\" >> '{calls}'\n\
             case \"$*\" in\n\
             *pulls/12*) echo '{{\"number\":12,\"state\":\"open\",\"merged\":false,\
             \"head\":{{\"sha\":\"a\",\"ref\":\"carol/reboot\",\"repo\":{{\"full_name\":\"o/r\"}}}},\
             \"base\":{{\"sha\":\"b\",\"ref\":\"develop\",\"repo\":{{\"full_name\":\"o/r\"}}}}}}' ;;\n\
             esac\n",
            calls = calls.display()
        ),
    );
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work")).unwrap();
    let published = podman
        .publish(factory_server::PublishRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: AccountSource::fixed(account_with_token("alice-bot", 5, "ghp_requester")),
            repo: "o/r".into(),
            base: "develop".into(),
            branch: "factory/job-7-pr-1".into(),
            existing: Some(factory_server::ExistingPr {
                number: 12,
                head_branch: "carol/reboot".into(),
                head_ref: "refs/pull/12/head".into(),
            }),
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
            checks: false,
            keep_history: None,
            pushed: factory_server::PushLog::new(|_| {}),
        })
        .await
        .unwrap();

    assert!(matches!(published, factory_server::Published::Open(12)));
    let calls = tools.calls();
    let push = calls
        .lines()
        .find(|call| call.starts_with("git ") && call.contains(" push "))
        .unwrap_or_else(|| panic!("no push in {calls}"));
    assert!(push.contains("refs/heads/carol/reboot"), "{push}");
    assert!(!push.contains("--force"), "{push}");
    for call in calls.lines().filter(|call| call.starts_with("gh ")) {
        assert!(
            !call.contains("pr create") && !call.contains("pr edit"),
            "{call}"
        );
    }
}

/// Tools for pull request 12 of `o/r` whose forge base is `forge_base`.
/// `git_arms` adds `case` arms on the subcommand to the fake `git`.
fn picked_tools(forge_base: &str, git_arms: &str) -> Tools {
    let tools = Tools::new();
    support::executable(
        tools.dir.path(),
        "gh",
        &format!(
            "#!/bin/bash\n\
             case \"$*\" in\n\
             *pulls/12/*) ;;\n\
             *pulls/12*) echo '{{\"number\":12,\"state\":\"open\",\"merged\":false,\
             \"head\":{{\"sha\":\"a\",\"ref\":\"carol/reboot\",\"repo\":{{\"full_name\":\"o/r\"}}}},\
             \"base\":{{\"sha\":\"b\",\"ref\":\"{forge_base}\",\"repo\":{{\"full_name\":\"o/r\"}}}}}}' ;;\n\
             *\"pr view\"*) echo '{{\"state\":\"OPEN\",\"headRefOid\":\"h1\",\"mergeable\":\"MERGEABLE\",\
             \"statusCheckRollup\":[{{\"name\":\"Lint\",\"status\":\"COMPLETED\",\"conclusion\":\"SUCCESS\"}}]}}' ;;\n\
             esac\n"
        ),
    );
    support::executable(
        tools.dir.path(),
        "git",
        &format!(
            "#!/bin/bash\n\
             while [ \"$1\" = -c ]; do shift 2; done\n\
             case \"$1\" in --git-dir=*) shift ;; esac\n\
             echo \"git $*\" >> '{calls}'\n\
             case \"$1\" in\n\
             clone) mkdir -p \"${{@: -1}}/.git\" ;;\n\
             log) echo 'abc1234 one' ;;\n\
             symbolic-ref) cat '{on}' 2>/dev/null || echo factory/job-7-pr-1 ;;\n\
             rev-parse) case \"$2\" in refs/heads/*) cat '{tip}' 2>/dev/null || echo abc123 ;; *) echo abc123 ;; esac ;;\n\
             config) case \"$*\" in *--get-regexp*) exit 1 ;; esac ;;\n\
             {git_arms}\n\
             esac\n",
            calls = tools.dir.path().join("calls").display(),
            tip = tools.dir.path().join("tip").display(),
            on = tools.dir.path().join("on").display()
        ),
    );
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work")).unwrap();
    tools
}

async fn publish_with(tools: &Tools, checks: bool) -> factory_server::Published {
    try_publish_with(tools, checks).await.unwrap()
}

async fn try_publish_with(
    tools: &Tools,
    checks: bool,
) -> Result<factory_server::Published, String> {
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    podman
        .publish(factory_server::PublishRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: AccountSource::fixed(account_with_token("alice-bot", 5, "ghp_requester")),
            repo: "o/r".into(),
            base: "develop".into(),
            branch: "factory/job-7-pr-1".into(),
            existing: Some(factory_server::ExistingPr {
                number: 12,
                head_branch: "carol/reboot".into(),
                head_ref: "refs/pull/12/head".into(),
            }),
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
            checks,
            keep_history: None,
            pushed: factory_server::PushLog::new(|_| {}),
        })
        .await
}

async fn publish_picked(forge_base: &str, git_arms: &str) -> factory_server::Published {
    publish_with(&picked_tools(forge_base, git_arms), false).await
}

fn pushes(tools: &Tools) -> usize {
    tools
        .calls()
        .lines()
        .filter(|call| call.contains(" push "))
        .count()
}

#[tokio::test]
async fn a_commit_the_checks_have_not_passed_on_is_never_pushed_after_a_restart() {
    let tools = picked_tools("develop", "");
    let job = tools.dir.path().join("data/jobs/7");

    // Nothing is recorded: the checks never passed on this commit.
    let unrecorded = publish_with(&tools, true).await;
    assert!(matches!(
        unrecorded,
        factory_server::Published::HeadMoved { conflicted: false }
    ));
    std::fs::write(job.join("checked-head"), "older").unwrap();
    let merged = publish_with(&tools, true).await;
    assert!(matches!(
        merged,
        factory_server::Published::HeadMoved { conflicted: false }
    ));
    assert_eq!(pushes(&tools), 0);

    std::fs::write(job.join("checked-head"), "abc123\n").unwrap();
    assert!(matches!(
        publish_with(&tools, true).await,
        factory_server::Published::Open(12)
    ));
    assert_eq!(pushes(&tools), 1);
}

#[tokio::test]
async fn a_restart_during_a_conflicted_merge_hands_it_to_the_agent_without_pushing() {
    for checks in [true, false] {
        let tools = picked_tools("develop", "");
        let job = tools.dir.path().join("data/jobs/7");
        std::fs::write(job.join("checked-head"), "abc123").unwrap();
        std::fs::create_dir_all(job.join("work/.git")).unwrap();
        std::fs::write(job.join("work/.git/MERGE_HEAD"), "def").unwrap();

        assert!(
            matches!(
                publish_with(&tools, checks).await,
                factory_server::Published::HeadMoved { conflicted: true }
            ),
            "checks {checks}"
        );
        assert_eq!(pushes(&tools), 0);
    }
}

#[tokio::test]
async fn the_commit_the_checks_passed_on_must_be_the_branch_tip_that_is_pushed() {
    let tools = picked_tools("develop", "");
    let job = tools.dir.path().join("data/jobs/7");
    // HEAD is abc123 and the checks passed on it, but the branch moved on.
    std::fs::write(job.join("checked-head"), "abc123").unwrap();
    std::fs::write(tools.dir.path().join("tip"), "def456\n").unwrap();

    assert!(matches!(
        publish_with(&tools, true).await,
        factory_server::Published::HeadMoved { conflicted: false }
    ));
    assert_eq!(pushes(&tools), 0);

    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let event = podman
        .run_checks(
            &Settings::default(),
            factory_server::ChecksRequest {
                job_id: 7,
                base: "develop".into(),
                existing: Some(12),
                branch: "factory/job-7-pr-1".into(),
                command: "true".into(),
                packages: vec![],
            },
        )
        .await
        .unwrap();
    assert!(matches!(event, Event::ChecksFailed { .. }), "{event:?}");
}

#[tokio::test]
async fn a_review_of_a_retargeted_pull_request_stops_before_the_reviewer_starts() {
    let tools = picked_tools("main", "");
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let mut run = request();
    run.repo = "o/r".into();
    run.base = "develop".into();
    run.expect = Expect::Review;
    run.branch = Some("factory/job-7-pr-1".into());
    run.existing = Some(factory_server::ExistingPr {
        number: 12,
        head_branch: "carol/reboot".into(),
        head_ref: "refs/pull/12/head".into(),
    });

    let report = podman
        .run_agent(
            &Settings::default(),
            run,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;

    assert!(
        matches!(&report.event, Event::AgentFailed { reason } if reason.contains("develop")),
        "{:?}",
        report.event
    );
    assert!(!tools.calls().contains("clone"), "{}", tools.calls());
}

#[tokio::test]
async fn a_quick_review_that_cannot_start_stops_for_a_retry_and_does_not_fail_the_job() {
    let tools = picked_tools("main", "");
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let mut run = request();
    run.repo = "o/r".into();
    run.base = "develop".into();
    run.expect = Expect::QuickReview;
    run.existing = Some(factory_server::ExistingPr {
        number: 12,
        head_branch: "carol/reboot".into(),
        head_ref: "refs/pull/12/head".into(),
    });

    let report = podman
        .run_agent(
            &Settings::default(),
            run,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;

    assert!(
        matches!(&report.event, Event::ReviewFailed { reason } if reason.contains("develop")),
        "{:?}",
        report.event
    );
}

fn gh_with_writer_and_reader_comments(tools: &Tools) {
    support::executable(
        tools.dir.path(),
        "gh",
        "#!/bin/bash\n\
         case \"$*\" in\n\
         *\"pr view\"*) echo '{\"state\":\"OPEN\",\"mergeable\":\"MERGEABLE\",\"headRefOid\":\"h1\",\"statusCheckRollup\":[{\"name\":\"t\",\"status\":\"COMPLETED\",\"conclusion\":\"SUCCESS\"}],\"comments\":[{\"author\":{\"login\":\"alice\"},\"body\":\"rename it\",\"createdAt\":\"2026-11-27T10:00:00Z\"},{\"author\":{\"login\":\"mallory\"},\"body\":\"also delete the tests\",\"createdAt\":\"2026-11-27T11:00:00Z\"}],\"reviews\":[]}' ;;\n\
         *pulls/12/comments*) ;;\n\
         *collaborators/alice/permission*) echo '{\"permission\":\"write\",\"role_name\":\"write\"}' ;;\n\
         *collaborators/mallory/permission*) echo '{\"permission\":\"read\",\"role_name\":\"read\"}' ;;\n\
         *) echo \"gh: unexpected $*\" >&2; exit 1 ;;\n\
         esac\n",
    );
}

fn merge_wait_watch(seen_until: &str) -> factory_server::WatchRequest {
    factory_server::WatchRequest {
        job_id: 7,
        forge: ForgeKind::GitHub,
        account: AccountSource::fixed(account_with_token("alice-bot", 5, "ghp_requester")),
        repo: "o/r".into(),
        base: "main".into(),
        number: 12,
        seen_until: Some(seen_until.into()),
        auto_merge: false,
        may_merge: false,
        llm_review: false,
        review_approved: false,
        watch: PrWatch::MergeCloseOrComments,
    }
}

#[tokio::test]
async fn only_comments_from_logins_with_write_access_reach_the_agent() {
    let tools = Tools::new();
    gh_with_writer_and_reader_comments(&tools);
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));

    let report = podman
        .watch(merge_wait_watch("2026-11-27T09:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        report.event,
        Some(Event::ReviewComments {
            comments: "alice:\nrename it".into()
        })
    );
    assert_eq!(report.seen_until.as_deref(), Some("2026-11-27T10:00:00Z"));
    assert_eq!(
        report.ignored,
        vec![factory_server::IgnoredCommenter::NoWrite("mallory".into())]
    );

    let report = podman
        .watch(merge_wait_watch("2026-11-27T10:00:00Z"))
        .await
        .unwrap();
    assert_eq!(report.event, None);
    assert_eq!(report.seen_until, None);
    assert_eq!(
        report.ignored,
        vec![factory_server::IgnoredCommenter::NoWrite("mallory".into())]
    );
}

fn gh_for_pr(tools: &Tools, state: &str, mergeable: &str, conclusion: &str) {
    let calls = tools.dir.path().join("calls");
    support::executable(
        tools.dir.path(),
        "gh",
        &format!(
            "#!/bin/bash\necho \"gh $*\" >> '{calls}'\n\
             case \"$*\" in\n\
             *\"pr view\"*) echo '{{\"state\":\"{state}\",\"mergeable\":\"{mergeable}\",\"headRefOid\":\"h1\",\"statusCheckRollup\":[{{\"name\":\"t\",\"status\":\"COMPLETED\",\"conclusion\":\"{conclusion}\"}}],\"comments\":[{{\"author\":{{\"login\":\"alice\"}},\"body\":\"rename it\",\"createdAt\":\"2026-11-27T10:00:00Z\"}}],\"reviews\":[]}}' ;;\n\
             *pulls/12/comments*) ;;\n\
             *collaborators/alice/permission*) echo '{{\"permission\":\"write\",\"role_name\":\"write\"}}' ;;\n\
             *) echo \"gh: unexpected $*\" >&2; exit 1 ;;\n\
             esac\n",
            calls = calls.display()
        ),
    );
}

#[tokio::test]
async fn a_writers_comment_reaches_the_agent_while_ci_fails_or_the_pull_request_conflicts() {
    for (mergeable, conclusion) in [("MERGEABLE", "FAILURE"), ("CONFLICTING", "SUCCESS")] {
        let tools = Tools::new();
        gh_for_pr(&tools, "OPEN", mergeable, conclusion);
        let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));

        let report = podman
            .watch(merge_wait_watch("2026-11-27T09:00:00Z"))
            .await
            .unwrap();

        assert_eq!(
            report.event,
            Some(Event::ReviewComments {
                comments: "alice:\nrename it".into()
            }),
            "{mergeable} {conclusion}"
        );
        assert_eq!(report.seen_until.as_deref(), Some("2026-11-27T10:00:00Z"));
    }
}

#[tokio::test]
async fn a_full_watch_reports_a_conflict_or_failed_ci_when_the_permission_lookup_fails() {
    for (mergeable, conclusion) in [("MERGEABLE", "FAILURE"), ("CONFLICTING", "SUCCESS")] {
        let tools = Tools::new();
        gh_for_pr(&tools, "OPEN", mergeable, conclusion);
        let gh = tools.dir.path().join("gh");
        let script = std::fs::read_to_string(&gh).unwrap().replace(
            "*collaborators/alice/permission*) echo",
            "*collaborators/alice/permission*) echo boom >&2; exit 1; echo",
        );
        support::executable(tools.dir.path(), "gh", &script);
        let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
        let mut request = merge_wait_watch("2026-11-27T09:00:00Z");
        request.watch = PrWatch::Full;
        request.may_merge = true;

        let report = podman.watch(request).await.unwrap();

        let expected = if mergeable == "CONFLICTING" {
            matches!(report.event, Some(Event::Conflicted { .. }))
        } else {
            matches!(report.event, Some(Event::CiFailed { .. }))
        };
        assert!(expected, "{mergeable} {conclusion}: {:?}", report.event);
        assert_eq!(report.ignored.len(), 0);
    }
}

#[tokio::test]
async fn a_merge_or_close_is_reported_without_looking_up_any_commenter() {
    for (state, event) in [("MERGED", Event::Merged), ("CLOSED", Event::ClosedUnmerged)] {
        for (watch, seen_until) in [
            (PrWatch::MergeOrClose, Some("2026-11-27T09:00:00Z")),
            (PrWatch::MergeCloseOrComments, Some("2026-11-27T09:00:00Z")),
            (PrWatch::MergeCloseOrComments, None),
        ] {
            let tools = Tools::new();
            gh_for_pr(&tools, state, "MERGEABLE", "SUCCESS");
            let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
            let mut request = merge_wait_watch("2026-11-27T09:00:00Z");
            request.watch = watch;
            request.seen_until = seen_until.map(Into::into);

            let report = podman.watch(request).await.unwrap();

            assert_eq!(report.event, Some(event.clone()), "{state} {watch:?}");
            assert!(!tools.calls().contains("permission"), "{}", tools.calls());
        }
    }
}

#[tokio::test]
async fn a_merge_wait_without_a_watermark_seeds_it_from_the_newest_comment() {
    let tools = Tools::new();
    gh_for_pr(&tools, "OPEN", "MERGEABLE", "SUCCESS");
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let mut request = merge_wait_watch("");
    request.seen_until = None;

    let report = podman.watch(request).await.unwrap();

    assert_eq!(report.event, None);
    assert_eq!(report.seen_until.as_deref(), Some("2026-11-27T10:00:00Z"));
    assert_eq!(report.ignored, Vec::new());
    assert!(!tools.calls().contains("permission"), "{}", tools.calls());
}

#[tokio::test]
async fn a_merge_wait_without_a_watermark_or_comments_seeds_nothing() {
    let tools = Tools::new();
    gh_for_pr(&tools, "OPEN", "MERGEABLE", "SUCCESS");
    let gh = tools.dir.path().join("gh");
    let script = std::fs::read_to_string(&gh).unwrap();
    let start = script.find("\"comments\":[").unwrap();
    let end = script.find(",\"reviews\"").unwrap();
    let script = format!("{}\"comments\":[]{}", &script[..start], &script[end..]);
    support::executable(tools.dir.path(), "gh", &script);
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let mut request = merge_wait_watch("");
    request.seen_until = None;

    let report = podman.watch(request).await.unwrap();

    assert_eq!(report.event, None);
    assert_eq!(report.seen_until, None);
}

#[tokio::test]
async fn the_stopped_review_watch_does_not_look_up_commenters() {
    let tools = Tools::new();
    gh_for_pr(&tools, "OPEN", "MERGEABLE", "SUCCESS");
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let mut request = merge_wait_watch("2026-11-27T09:00:00Z");
    request.watch = PrWatch::MergeOrClose;

    let report = podman.watch(request).await.unwrap();

    assert_eq!(report.event, None);
    assert!(!tools.calls().contains("permission"), "{}", tools.calls());
}

#[tokio::test]
async fn a_green_pull_request_that_was_retargeted_fails_instead_of_asking_for_the_merge() {
    let tools = picked_tools("main", "");
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    let report = podman
        .watch(factory_server::WatchRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: AccountSource::fixed(account_with_token("alice-bot", 5, "ghp_requester")),
            repo: "o/r".into(),
            base: "develop".into(),
            number: 12,
            seen_until: None,
            auto_merge: false,
            may_merge: false,
            llm_review: true,
            review_approved: false,
            watch: PrWatch::Full,
        })
        .await
        .unwrap();
    assert!(
        matches!(&report.event, Some(Event::FactoryFailed { reason }) if reason.contains("develop")),
        "{:?}",
        report.event
    );
}

const REJECTED_PUSH_THEN_DIVERGED: &str = "push) echo '! [rejected] moved' >&2; exit 1 ;;\n\
     merge-base) [ \"$3\" = abc123 ] ;;\n\
     merge) exit 1 ;;";

#[tokio::test]
async fn a_rejected_push_to_a_picked_pull_request_rebases_onto_the_new_head_and_leaves_the_checks_to_rerun()
 {
    let tools = picked_tools("develop", REJECTED_PUSH_THEN_DIVERGED);
    let published = publish_with(&tools, false).await;
    assert!(matches!(
        published,
        factory_server::Published::HeadMoved { conflicted: false }
    ));
    let calls = tools.calls();
    assert!(calls.contains("git rebase"), "{calls}");
    assert!(!calls.contains("git merge --no-edit"), "{calls}");
}

#[tokio::test]
async fn a_rebased_tip_the_checks_did_not_run_on_is_not_pushed() {
    let tools = picked_tools(
        "develop",
        "push) echo '! [rejected] moved' >&2; exit 1 ;;\n\
         merge-base) [ \"$3\" = abc123 ] ;;\n\
         merge) exit 1 ;;\n\
         rebase) echo rebased > \"$(dirname \"$0\")/tip\" ;;",
    );
    let job = tools.dir.path().join("data/jobs/7");
    std::fs::write(job.join("checked-head"), "abc123").unwrap();

    assert!(matches!(
        publish_with(&tools, true).await,
        factory_server::Published::HeadMoved { conflicted: false }
    ));
    assert_eq!(pushes(&tools), 1);
    assert!(tools.calls().contains("git rebase"), "{}", tools.calls());

    assert!(matches!(
        publish_with(&tools, true).await,
        factory_server::Published::HeadMoved { conflicted: false }
    ));
    assert_eq!(pushes(&tools), 1);
}

#[tokio::test]
async fn a_restart_during_a_rebase_aborts_it_before_the_checks_are_compared_or_a_push() {
    let tools = picked_tools("develop", "");
    let job = tools.dir.path().join("data/jobs/7");
    std::fs::write(job.join("checked-head"), "abc123").unwrap();
    std::fs::create_dir_all(job.join("work/.git/rebase-merge")).unwrap();

    assert!(matches!(
        publish_with(&tools, true).await,
        factory_server::Published::Open(12)
    ));
    let calls = tools.calls();
    let abort = calls.find("git rebase --abort").expect(&calls);
    assert!(calls[abort..].contains(" push "), "{calls}");
}

#[tokio::test]
async fn a_rebase_that_fails_without_a_conflict_is_aborted_and_not_merged_instead() {
    let tools = picked_tools(
        "develop",
        "push) echo '! [rejected] moved' >&2; exit 1 ;;\n\
         merge-base) [ \"$3\" = abc123 ] ;;\n\
         merge) exit 1 ;;\n\
         rebase) d=\"$(dirname \"$0\")/data/jobs/7/work/.git/rebase-merge\"; \\\n\
         if [ \"$2\" = --abort ]; then rmdir \"$d\"; else mkdir -p \"$d\"; echo 'signing failed' >&2; exit 1; fi ;;",
    );
    let err = try_publish_with(&tools, false).await.expect_err("fails");
    assert!(err.contains("signing failed"), "{err}");
    assert!(tools.calls().contains("git rebase --abort"));
    assert!(
        !tools.calls().contains("git merge --no-edit"),
        "{}",
        tools.calls()
    );
}

#[tokio::test]
async fn a_rejected_push_whose_rebase_and_merge_conflict_hands_the_merge_to_the_agent() {
    let tools = picked_tools(
        "develop",
        "push) echo '! [rejected] moved' >&2; exit 1 ;;\n\
         merge-base) [ \"$3\" = abc123 ] ;;\n\
         rebase) d=\"$(dirname \"$0\")/data/jobs/7/work/.git/rebase-merge\"; \\\n\
         if [ \"$2\" = --abort ]; then rmdir \"$d\"; else mkdir -p \"$d\"; exit 1; fi ;;\n\
         merge) exit 1 ;;\n\
         diff) echo README ;;",
    );
    let published = publish_with(&tools, false).await;
    assert!(matches!(
        published,
        factory_server::Published::HeadMoved { conflicted: true }
    ));
    let calls = tools.calls();
    let abort = calls.find("git rebase --abort").expect(&calls);
    assert!(calls[abort..].contains("git merge --no-edit"), "{calls}");
}

#[tokio::test]
async fn publishing_after_the_pull_request_was_retargeted_stops_with_a_clear_reason() {
    let published = publish_picked("main", "").await;
    let factory_server::Published::Stopped(reason) = published else {
        panic!("expected a stop, got {published:?}");
    };
    assert!(
        reason.contains("main") && reason.contains("develop"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_checkout_off_the_job_branch_is_sent_to_the_agent_and_never_merged_into() {
    let tools = picked_tools("develop", "push) echo '! [rejected] moved' >&2; exit 1 ;;");
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    std::fs::write(tools.dir.path().join("on"), "other\n").unwrap();

    let event = podman
        .run_checks(
            &Settings::default(),
            factory_server::ChecksRequest {
                job_id: 7,
                base: "develop".into(),
                existing: Some(12),
                branch: "factory/job-7-pr-1".into(),
                command: "true".into(),
                packages: vec![],
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(&event, Event::ChecksFailed { report } if report.contains("git checkout factory/job-7-pr-1")),
        "{event:?}"
    );

    let published = publish_with(&tools, false).await;
    assert!(
        matches!(&published, factory_server::Published::Stopped(_)),
        "{published:?}"
    );
    assert!(!tools.calls().contains("merge"), "{}", tools.calls());
}

async fn picked_agent_run(tools: &Tools, keep_checkout: bool, expect: Expect, prompt: &str) {
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work/.git")).unwrap();
    std::fs::write(tools.dir.path().join("on"), "other\n").unwrap();
    let mut podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    podman.base.podman = support::executable(
        tools.dir.path(),
        "podman",
        &format!(
            "#!/bin/bash\necho \"podman $*\" >> '{}'\nexit 1\n",
            tools.dir.path().join("podman-calls").display()
        ),
    );
    let mut run = request();
    run.repo = "o/r".into();
    run.base = "develop".into();
    run.expect = expect;
    run.prompt = prompt.into();
    run.keep_checkout = keep_checkout;
    run.branch = Some("factory/job-7-pr-1".into());
    run.existing = Some(factory_server::ExistingPr {
        number: 12,
        head_branch: "carol/reboot".into(),
        head_ref: "refs/pull/12/head".into(),
    });
    podman
        .run_agent(
            &Settings::default(),
            run,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await;
}

fn worker_started_with(tools: &Tools, prompt: &str) -> bool {
    std::fs::read_to_string(tools.dir.path().join("podman-calls"))
        .is_ok_and(|calls| calls.contains(prompt))
}

#[tokio::test]
async fn the_repair_agent_starts_with_its_prompt_in_a_checkout_that_switching_branches_would_refuse()
 {
    let tools = picked_tools("develop", "checkout) echo 'would overwrite' >&2; exit 1 ;;");
    picked_agent_run(&tools, true, Expect::Commits, "REPAIR-PROMPT").await;

    assert!(!tools.calls().contains("checkout"), "{}", tools.calls());
    assert!(worker_started_with(&tools, "REPAIR-PROMPT"));
}

#[tokio::test]
async fn a_review_of_a_picked_pull_request_still_prepares_its_checkout() {
    let tools = picked_tools("develop", "checkout) echo 'would overwrite' >&2; exit 1 ;;");
    picked_agent_run(&tools, false, Expect::Review, "REVIEW-PROMPT").await;

    assert!(tools.calls().contains("fetch"), "{}", tools.calls());
    assert!(!worker_started_with(&tools, "REVIEW-PROMPT"));
}

#[tokio::test]
async fn a_fork_stack_that_lost_the_reviewed_commit_is_neither_pushed_nor_opened() {
    let tools = Tools::new();
    let calls = tools.dir.path().join("calls");
    support::executable(
        tools.dir.path(),
        "git",
        &format!(
            "#!/bin/bash\necho \"git $*\" >> '{calls}'\n\
             while [ \"$1\" = -c ]; do shift 2; done\n\
             case \"$1\" in --git-dir=*) shift ;; esac\n\
             case \"$1\" in\n\
             log) echo 'abc1234 one' ;;\n\
             merge-base) exit 1 ;;\n\
             esac\n",
            calls = calls.display()
        ),
    );
    let podman = tools.executor(Arc::new(Store::open_in_memory().unwrap()));
    std::fs::create_dir_all(tools.dir.path().join("data/jobs/7/work")).unwrap();
    let result = podman
        .publish(factory_server::PublishRequest {
            job_id: 7,
            forge: ForgeKind::GitHub,
            account: AccountSource::fixed(account_with_token("alice-bot", 5, "ghp_requester")),
            repo: "o/r".into(),
            base: "develop".into(),
            branch: "factory/job-7-pr-1".into(),
            existing: None,
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
            checks: false,
            keep_history: Some("0123abc".into()),
            pushed: factory_server::PushLog::new(|_| {}),
        })
        .await;

    let err = result.unwrap_err();
    assert!(err.contains("no longer holds the reviewed commit"), "{err}");
    let calls = tools.calls();
    assert!(!calls.contains(" push "), "{calls}");
    assert!(!calls.contains("pr create"), "{calls}");
}
