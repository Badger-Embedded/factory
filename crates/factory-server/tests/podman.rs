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
use std::sync::Arc;

use factory_core::Event;
use factory_runner::{Account, Expect, ForgeKind, Git, GitHub, WorkerConfig};
use factory_server::{
    AgentRegistry, AgentRequest, ClaudeToken, CodexLogin, Executor, Forges, GhRepos, LocalSecrets,
    MASTER_KEY_LEN, PlanUsageCache, PodmanExecutor, Rebase, SecretStore, Settings, Store,
};

/// A subscription `auth.json`, shaped like the one `codex login` writes.
const AUTH: &str = r#"{"tokens":{"access_token":"at-1","refresh_token":"rt-1"},"last_refresh":"2026-11-20T00:00:00Z"}"#;

fn registry(secrets: &Arc<dyn SecretStore>) -> Arc<AgentRegistry> {
    Arc::new(AgentRegistry::new(
        Arc::new(ClaudeToken::new(Arc::clone(secrets), None)),
        Arc::new(CodexLogin::new(Arc::clone(secrets))),
        Arc::clone(secrets),
    ))
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
        account: Account::default(),
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
        .merge_renovate(ForgeKind::GitHub, &Account::default(), "o/r", 5, "checked1")
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
        account: Account::default(),
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
            &Account::default(),
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
    let account = Account::default();

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
