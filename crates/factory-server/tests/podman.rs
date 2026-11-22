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
use factory_runner::{Expect, Git, GitHub, WorkerConfig};
use factory_server::{
    AgentRegistry, AgentRequest, ClaudeToken, CodexLogin, Executor, LocalSecrets, MASTER_KEY_LEN,
    PlanUsageCache, PodmanExecutor, SecretStore, Settings, Store,
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

fn executor(registry: Arc<AgentRegistry>, data_dir: PathBuf, store: Arc<Store>) -> PodmanExecutor {
    PodmanExecutor {
        base: WorkerConfig::default(),
        // A missing git binary makes the checkout step fail, so a run that gets
        // past the token gate fails there with a different reason.
        git: Git {
            git: PathBuf::from("/nonexistent/git"),
            ..Git::default()
        },
        github: GitHub::default(),
        data_dir,
        registry,
        store,
        plan_usage: Arc::new(PlanUsageCache::new()),
    }
}

fn request() -> AgentRequest {
    AgentRequest {
        job_id: 7,
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
