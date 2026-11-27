//! Tests for the executor's part of an `OpenRouter` run: one key check before
//! any checkout or container, the failure it gives for a rejected key or an
//! unreachable host, and a key that reaches podman by name only. A fake `git`
//! and `podman` record their calls.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use factory_core::{Agent, Cli, Event};
use factory_runner::{
    Account, Expect, ForgeKind, Git, GitHub, LogLine, ProviderKind, ProviderTarget, RunReport,
    WorkerConfig,
};
use factory_server::{
    AccountSource, AgentRegistry, AgentRequest, ClaudeToken, CodexLogin, Executor, Forges, GhRepos,
    HttpLocalServerProbe, KeyOwner, LocalSecrets, MASTER_KEY_LEN, ModelsError, OpenRouterApi,
    OpenRouterKeyFuture, OpenRouterModelsFuture, PlanUsageCache, PodmanExecutor, ProviderRequest,
    SecretStore, Settings, Store,
};

mod support;

const KEY: &str = "sk-or-v1-run-key-789";

struct FakeApi {
    answer: Result<(), ModelsError>,
    seen: Mutex<Vec<String>>,
}

impl OpenRouterApi for FakeApi {
    fn models<'a>(&'a self, _: Option<&'a str>) -> OpenRouterModelsFuture<'a> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn check_key<'a>(&'a self, key: &'a str) -> OpenRouterKeyFuture<'a> {
        self.seen.lock().unwrap().push(key.to_owned());
        Box::pin(std::future::ready(self.answer.clone()))
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    api: Arc<FakeApi>,
}

impl Fixture {
    fn new(answer: Result<(), ModelsError>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        support::executable(
            dir.path(),
            "git",
            &format!(
                "#!/bin/bash\necho git >> '{calls}'\n\
                 while [ \"$1\" = -c ]; do shift 2; done\n\
                 case \"$1\" in --git-dir=*) shift ;; esac\n\
                 case \"$1\" in\n\
                 clone) mkdir -p \"${{@: -1}}/.git\" ;;\n\
                 config) case \"$*\" in *--get-regexp*) exit 1 ;; esac ;;\n\
                 esac\n",
                calls = dir.path().join("calls").display()
            ),
        );
        support::executable(
            dir.path(),
            "podman",
            &format!(
                "#!/bin/bash\necho \"podman $*\" >> '{calls}'\necho '{{}}'\n",
                calls = dir.path().join("calls").display()
            ),
        );
        Fixture {
            dir,
            store: Arc::new(Store::open_in_memory().unwrap()),
            api: Arc::new(FakeApi {
                answer,
                seen: Mutex::new(Vec::new()),
            }),
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn executor(&self) -> PodmanExecutor {
        let secrets: Arc<dyn SecretStore> = Arc::new(LocalSecrets::new(
            &[1; MASTER_KEY_LEN],
            Arc::clone(&self.store),
        ));
        PodmanExecutor {
            base: WorkerConfig {
                podman: self.dir.path().join("podman"),
                ..WorkerConfig::default()
            },
            git: Git {
                git: self.dir.path().join("git"),
            },
            forges: Forges::new(
                Arc::new(GitHub::default()),
                Arc::new(GhRepos {
                    gh: PathBuf::from("gh"),
                }),
            ),
            data_dir: self.dir.path().join("data"),
            registry: Arc::new(AgentRegistry::new(
                Arc::new(ClaudeToken::new(Arc::clone(&secrets), None)),
                Arc::new(CodexLogin::new(Arc::clone(&secrets))),
                secrets,
            )),
            store: Arc::clone(&self.store),
            plan_usage: Arc::new(PlanUsageCache::new()),
            local_probe: Arc::new(HttpLocalServerProbe::new()),
            openrouter: Arc::clone(&self.api) as Arc<dyn OpenRouterApi>,
        }
    }

    async fn run(&self, request: AgentRequest) -> (RunReport, Vec<String>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let report = self
            .executor()
            .run_agent(
                &Settings::default(),
                request,
                Box::new(move |line| {
                    if let LogLine::Other(text) = line {
                        sink.lock().unwrap().push(text);
                    }
                }),
                Box::new(|_| {}),
            )
            .await;
        let lines = lines.lock().unwrap().clone();
        (report, lines)
    }
}

fn request(job_id: i64, cli: Cli, key_from: KeyOwner) -> AgentRequest {
    AgentRequest {
        job_id,
        forge: ForgeKind::GitHub,
        account: AccountSource::fixed(Account {
            token: Some("project-token".into()),
            login: "factory-bot".into(),
            name: "factory-bot".into(),
            email: "1+factory-bot@users.noreply.github.com".into(),
        }),
        repo: "owner/name".into(),
        base: "main".into(),
        branch: None,
        existing: None,
        merge_base: false,
        keep_checkout: false,
        session_id: "0b6f5c1e-8d1a-4c3e-9a57-3c1f2a9d7e10".into(),
        agent_session: None,
        agent: Agent::OpenRouter,
        expect: Expect::Spec,
        prompt: "Write the spec.".into(),
        model: Some("anthropic/claude-sonnet-4.5".into()),
        effort: None,
        mcp: None,
        packages: Vec::new(),
        provider: Some(ProviderRequest {
            target: ProviderTarget {
                kind: ProviderKind::OpenRouter,
                cli,
                base_url: "https://openrouter.ai/api".into(),
                key: Some(KEY.into()),
            },
            key_from,
            setting_cli: cli,
            session: factory_core::Session::Planning,
        }),
    }
}

async fn failure(answer: Result<(), ModelsError>, key_from: KeyOwner) -> (String, Fixture) {
    let fixture = Fixture::new(answer);
    let (report, _) = fixture.run(request(7, Cli::Codex, key_from)).await;
    let Event::AgentFailed { reason } = report.event else {
        panic!("expected AgentFailed");
    };
    (reason, fixture)
}

#[tokio::test]
async fn a_rejected_personal_key_fails_the_run_and_names_its_owner() {
    let (reason, fixture) = failure(
        Err(ModelsError::Status(401)),
        KeyOwner::Login("alice".into()),
    )
    .await;

    assert!(
        reason.starts_with("OpenRouter rejected the OpenRouter API key of alice."),
        "{reason}"
    );
    assert!(!reason.contains(KEY));
    assert_eq!(fixture.calls(), Vec::<String>::new());
    assert_eq!(*fixture.api.seen.lock().unwrap(), [KEY]);
}

#[tokio::test]
async fn a_rejected_factory_key_fails_the_run_before_any_checkout() {
    let (reason, fixture) = failure(Err(ModelsError::Status(403)), KeyOwner::Factory).await;

    assert!(
        reason.starts_with("OpenRouter rejected the factory's OpenRouter API key."),
        "{reason}"
    );
    assert_eq!(fixture.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn an_unreachable_openrouter_fails_the_run() {
    for error in [ModelsError::Connect, ModelsError::Timeout] {
        let (reason, fixture) = failure(Err(error), KeyOwner::Factory).await;
        assert_eq!(reason, "The factory cannot reach OpenRouter.");
        assert_eq!(fixture.calls(), Vec::<String>::new());
    }
}

#[tokio::test]
async fn an_accepted_key_starts_the_worker_with_the_key_by_name_only() {
    for (cli, name) in [
        (Cli::ClaudeCode, "ANTHROPIC_AUTH_TOKEN"),
        (Cli::Codex, "FACTORY_OPENROUTER_API_KEY"),
        (Cli::Copilot, "COPILOT_PROVIDER_API_KEY"),
    ] {
        let fixture = Fixture::new(Ok(()));
        let job = fixture
            .store
            .create("maestro", "alice", "Add a button")
            .unwrap();
        fixture
            .store
            .session_uuid(job, factory_core::Session::Planning, Agent::OpenRouter)
            .unwrap();
        let (_, lines) = fixture.run(request(job, cli, KeyOwner::Factory)).await;

        let calls = fixture.calls();
        assert!(calls.iter().any(|call| call.starts_with("podman run")));
        let run = calls
            .iter()
            .find(|call| call.starts_with("podman run"))
            .unwrap();
        assert!(run.contains(&format!("--env {name}")), "{run}");
        assert!(!run.contains(KEY), "{run}");
        assert!(lines.iter().all(|line| !line.contains(KEY)));
        let dir = format!("openrouter-{}", cli.id());
        assert!(
            fixture
                .dir
                .path()
                .join("data/jobs")
                .join(job.to_string())
                .join(dir)
                .is_dir()
        );
        assert_eq!(
            fixture
                .store
                .session_provider_cli(job, factory_core::Session::Planning)
                .unwrap(),
            Some(cli)
        );
    }
}
