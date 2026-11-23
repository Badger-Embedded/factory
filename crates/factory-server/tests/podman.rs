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

use factory_core::Event;
use factory_runner::{Account, Expect, Forge, ForgeKind, Git, GitHub, WorkerConfig};
use factory_server::{
    AccountSource, AgentRegistry, AgentRequest, ClaudeToken, CodexLogin, Executor, Forges, GhRepos,
    LocalSecrets, MASTER_KEY_LEN, Operation, Owner, PlanUsageCache, PodmanExecutor, Rebase,
    SecretStore, Settings, Store,
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
        session_id: "0b6f5c1e-8d1a-4c3e-9a57-3c1f2a9d7e10".into(),
        agent_session: None,
        agent: factory_core::DEFAULT_AGENT,
        expect: Expect::Spec,
        prompt: "Write the spec.".into(),
        model: None,
        effort: None,
        mcp: None,
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
            "#!/bin/bash\necho \"$@\" >> '{}'\necho '{{\"headRefOid\":\"unchecked2\"}}'\n",
            calls.display()
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
            ForgeKind::GitHub,
            &AccountSource::fixed(test_account()),
            "o/r",
            5,
            "checked1",
        )
        .await
        .unwrap();

    let calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(calls.lines().count(), 2, "{calls}");
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
            "#!/bin/bash\necho \"$@\" >> '{}'\ncase \"$1 $2\" in\n\
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
        base: "main".into(),
        number: 5,
        manifest_files: vec!["snap/snapcraft.yaml".into()],
        build_config_files: vec!["CMakeLists.txt".into()],
        verdict_head: verdict_head.map(str::to_owned),
        from: range.map(|(from, _)| from.to_owned()),
        to: range.map(|(_, to)| to.to_owned()),
    }
}

const COMPARE_BODY: &str = "Bumps app. [Compare Source](https://github.com/up/app/compare/v1...v2)";

#[tokio::test]
async fn a_merge_that_touches_no_manifest_never_downloads_its_diff() {
    let dir = tempfile::tempdir().unwrap();
    let (podman, calls) = gh_executor(dir.path(), &view("h1", "", &["Cargo.lock"]), 300_000);
    let manifests = vec!["snap/snapcraft.yaml".to_owned()];

    let event = podman
        .renovate_merged(
            ForgeKind::GitHub,
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
        .request_rebase(ForgeKind::GitHub, &account, "o/r", 5, "h0")
        .await;
    assert_eq!(moved, Ok(Rebase::HeadMoved));
    assert!(!std::fs::read_to_string(&calls).unwrap().contains("PATCH"));

    let requested = podman
        .request_rebase(ForgeKind::GitHub, &account, "o/r", 5, "h1")
        .await;
    assert_eq!(requested, Ok(Rebase::Requested));
    assert!(std::fs::read_to_string(&calls).unwrap().contains("PATCH"));

    let dir = tempfile::tempdir().unwrap();
    let (podman, _) = gh_executor(dir.path(), &view("h1", "no checkbox", &[]), 0);
    let missing = podman
        .request_rebase(ForgeKind::GitHub, &account, "o/r", 5, "h1")
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
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
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
            .forge(ForgeKind::GitHub)
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
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
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
            description: factory_core::PrDescription {
                title: "t".into(),
                body: "b".into(),
            },
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
            llm_review: false,
            review_approved: false,
            watch_only: false,
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
        .comment(ForgeKind::GitHub, &source, "o/r", 5, "hi")
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
