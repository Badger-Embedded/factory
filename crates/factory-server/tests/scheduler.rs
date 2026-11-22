use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use factory_core::{
    Agent, AgentResult, Ask, BranchSummary, Event, Gates, MAX_FAILED_ROUNDS, MAX_REVIEW_ROUNDS,
    PrDescription, Question, RenovateCi, RenovateWait, ReviewState, Stage, Turn,
};
use factory_runner::{
    Expect, LogLine, McpUnavailable, Observation, RenovatePr, Rerun, RunReport, TokenUsage,
    is_renovate_login,
};
use factory_server::{
    AgentRequest, ChecksRequest, CodexPrice, Executor, JobFile, LogSink, McpProjectSettings,
    McpRuntime, McpStages, McpWarnings, Project, Projects, PublishRequest, Published, ReadyAction,
    RenovateWatchRequest, RerunRequest, RunTokens, Scheduler, SecretRef, SecretStore, SessionSink,
    Settings, StageAgents, StageEfforts, StageModels, Store, SummaryRequest, UnavailableSecrets,
    WatchReport, WatchRequest, WorkerSlots, format_cost, ready_action, watch_only_event,
};
use tokio::sync::{Notify, Semaphore};

fn ready() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.".into(),
    }
}

type Respond = Box<dyn Fn(&AgentRequest) -> Event + Send + Sync>;

/// Holds each run until the test adds a permit to `gate`, then answers with
/// `respond`.
struct FakeExecutor {
    gate: Semaphore,
    respond: Respond,
    requests: Mutex<Vec<AgentRequest>>,
    costs: Mutex<VecDeque<Option<f64>>>,
    /// The token usage each run reports instead of a dollar cost, the way a
    /// Codex run does. A run with none queued reports no usage, as Claude does.
    token_usages: Mutex<VecDeque<TokenUsage>>,
    /// The MCP servers each run reports as not connected in its init line.
    unavailable: Mutex<VecDeque<Vec<McpUnavailable>>>,
    /// The agent-reported session ids each run streams back, such as Codex's
    /// thread id, in run order. A run with none reports no id, as Claude does.
    sessions: Mutex<VecDeque<String>>,
    stopped: Mutex<Vec<i64>>,
    stop_gate: Semaphore,
    watch_gate: Semaphore,
    checks: Mutex<VecDeque<Result<Event, String>>>,
    checks_run: Mutex<Vec<ChecksRequest>>,
    summarized: Mutex<Vec<SummaryRequest>>,
    publishes: Mutex<VecDeque<Result<Published, String>>>,
    published: Mutex<Vec<PublishRequest>>,
    watches: Mutex<VecDeque<Result<WatchReport, String>>>,
    watched: Mutex<Vec<WatchRequest>>,
    reruns: Mutex<VecDeque<Result<Rerun, String>>>,
    rerun_requests: Mutex<Vec<RerunRequest>>,
    /// Open pull requests as `(author, base, pull request)`.
    renovate_open: Mutex<Vec<(String, String, RenovatePr)>>,
    renovate_listed: Mutex<Vec<(String, String)>>,
    /// What the next look at a Renovate pull request finds.
    renovate_now: Mutex<RenovateNow>,
    renovate_watched: Mutex<Vec<RenovateWatchRequest>>,
    comments: Mutex<Vec<(u64, String)>>,
    /// Each approval and squash merge of `(number, head)`.
    merged: Mutex<Vec<(u64, String)>>,
    merge_failures: Mutex<VecDeque<RenovateNow>>,
    approved: Mutex<Vec<(u64, String)>>,
}

/// The state the fake reports for the Renovate pull request.
#[derive(Clone)]
enum RenovateNow {
    Nothing,
    Ci(&'static str, RenovateCi),
    Merged,
    Closed,
}

impl FakeExecutor {
    fn new(respond: impl Fn(&AgentRequest) -> Event + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FakeExecutor {
            gate: Semaphore::new(0),
            respond: Box::new(respond),
            requests: Mutex::new(Vec::new()),
            costs: Mutex::new(VecDeque::new()),
            token_usages: Mutex::new(VecDeque::new()),
            unavailable: Mutex::new(VecDeque::new()),
            sessions: Mutex::new(VecDeque::new()),
            stopped: Mutex::new(Vec::new()),
            stop_gate: Semaphore::new(Semaphore::MAX_PERMITS),
            watch_gate: Semaphore::new(Semaphore::MAX_PERMITS),
            checks: Mutex::new(VecDeque::new()),
            checks_run: Mutex::new(Vec::new()),
            summarized: Mutex::new(Vec::new()),
            publishes: Mutex::new(VecDeque::new()),
            published: Mutex::new(Vec::new()),
            watches: Mutex::new(VecDeque::new()),
            watched: Mutex::new(Vec::new()),
            reruns: Mutex::new(VecDeque::new()),
            rerun_requests: Mutex::new(Vec::new()),
            renovate_open: Mutex::new(Vec::new()),
            renovate_listed: Mutex::new(Vec::new()),
            renovate_now: Mutex::new(RenovateNow::Nothing),
            renovate_watched: Mutex::new(Vec::new()),
            comments: Mutex::new(Vec::new()),
            merged: Mutex::new(Vec::new()),
            merge_failures: Mutex::new(VecDeque::new()),
            approved: Mutex::new(Vec::new()),
        })
    }

    fn hold_stops(&self) {
        self.stop_gate.forget_permits(Semaphore::MAX_PERMITS);
    }

    fn hold_watches(&self) {
        self.watch_gate.forget_permits(Semaphore::MAX_PERMITS);
    }

    fn release(&self, runs: usize) {
        self.gate.add_permits(runs);
    }

    /// Makes the next run report `servers` as not connected in its init line,
    /// the way a worker's stream-json `init` line does.
    fn report_unavailable(&self, servers: Vec<McpUnavailable>) {
        self.unavailable.lock().unwrap().push_back(servers);
    }

    /// Makes the next run stream back `id` as the agent's own session id, the
    /// way a Codex run reports its thread id on `thread.started`.
    fn report_session(&self, id: &str) {
        self.sessions.lock().unwrap().push_back(id.to_owned());
    }

    fn requests(&self) -> Vec<AgentRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Executor for FakeExecutor {
    async fn run_agent(
        &self,
        _settings: &Settings,
        request: AgentRequest,
        mut log: LogSink,
        mut session: SessionSink,
    ) -> RunReport {
        self.requests.lock().unwrap().push(request.clone());
        self.gate.acquire().await.unwrap().forget();
        // Stream back the agent's own session id, if the test queued one, before
        // the run ends, the way a Codex run reports its thread id mid-run.
        if let Some(id) = self.sessions.lock().unwrap().pop_front() {
            session(id);
        }
        log(LogLine::Text("Reading the repository.".into()));
        // A run with queued token usage reports it and no dollar cost, the way a
        // Codex run does; otherwise it reports a dollar cost like Claude.
        let token_usage = self.token_usages.lock().unwrap().pop_front();
        let cost_usd = if token_usage.is_some() {
            None
        } else {
            self.costs.lock().unwrap().pop_front().unwrap_or(Some(0.12))
        };
        RunReport {
            event: (self.respond)(&request),
            session_id: Some(request.session_id),
            cost_usd,
            mcp_unavailable: self
                .unavailable
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default(),
            credential: None,
            token_usage,
        }
    }

    async fn run_checks(
        &self,
        _settings: &Settings,
        request: ChecksRequest,
    ) -> Result<Event, String> {
        tokio::task::yield_now().await;
        self.checks_run.lock().unwrap().push(request);
        self.checks
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(Event::ChecksPassed))
    }

    async fn summarize(&self, request: SummaryRequest) -> Result<BranchSummary, String> {
        tokio::task::yield_now().await;
        self.summarized.lock().unwrap().push(request);
        Ok(BranchSummary {
            commits: vec!["abc1234 maestrod: add a reboot endpoint".into()],
            diff_stat: " src/main.rs | 4 ++++".into(),
        })
    }

    async fn publish(&self, request: PublishRequest) -> Result<Published, String> {
        tokio::task::yield_now().await;
        self.published.lock().unwrap().push(request);
        self.publishes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(Published::Open(12)))
    }

    async fn watch(&self, request: WatchRequest) -> Result<WatchReport, String> {
        self.watch_gate.acquire().await.unwrap().forget();
        tokio::task::yield_now().await;
        self.watched.lock().unwrap().push(request);
        self.watches
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(WatchReport {
                event: None,
                seen_until: None,
                merged_by_factory: false,
            }))
    }

    async fn rerun(&self, request: RerunRequest) -> Result<Rerun, String> {
        tokio::task::yield_now().await;
        self.rerun_requests.lock().unwrap().push(request);
        self.reruns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(Rerun::Started(vec![7])))
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn renovate_prs(&self, repo: &str, base: &str) -> Result<Vec<RenovatePr>, String> {
        self.renovate_listed
            .lock()
            .unwrap()
            .push((repo.to_owned(), base.to_owned()));
        Ok(self
            .renovate_open
            .lock()
            .unwrap()
            .iter()
            .filter(|(author, pr_base, _)| is_renovate_login(author) && pr_base == base)
            .map(|(_, _, pr)| pr.clone())
            .collect())
    }

    async fn watch_renovate(&self, request: RenovateWatchRequest) -> Result<Option<Event>, String> {
        tokio::task::yield_now().await;
        let now = self.renovate_now.lock().unwrap().clone();
        self.renovate_watched.lock().unwrap().push(request.clone());
        Ok(match now {
            RenovateNow::Nothing => None,
            RenovateNow::Merged => Some(Event::Merged),
            RenovateNow::Closed => Some(Event::ClosedUnmerged),
            RenovateNow::Ci(head, ci) => {
                if request.watch_only && request.known_head.as_deref() == Some(head) {
                    return Ok(None);
                }
                let ci = match ci {
                    RenovateCi::Passed { details }
                        if request.verdict_head.as_deref() == Some(head) =>
                    {
                        drop(details);
                        RenovateCi::Passed { details: None }
                    }
                    ci => ci,
                };
                Some(Event::RenovateObserved {
                    head: head.into(),
                    ci,
                    limits: request.limits,
                    auto_merge: request.auto_merge,
                })
            }
        })
    }

    async fn comment(&self, _repo: &str, number: u64, body: &str) -> Result<(), String> {
        tokio::task::yield_now().await;
        self.comments
            .lock()
            .unwrap()
            .push((number, body.to_owned()));
        Ok(())
    }

    async fn merge_renovate(&self, _repo: &str, number: u64, head: &str) -> Result<(), String> {
        tokio::task::yield_now().await;
        self.approved
            .lock()
            .unwrap()
            .push((number, head.to_owned()));
        if let Some(now) = self.merge_failures.lock().unwrap().pop_front() {
            *self.renovate_now.lock().unwrap() = now;
            return Err("the head commit changed".into());
        }
        self.merged.lock().unwrap().push((number, head.to_owned()));
        Ok(())
    }

    async fn stop(&self, job_id: i64) {
        self.stop_gate.acquire().await.unwrap().forget();
        self.stopped.lock().unwrap().push(job_id);
    }
}

fn questions() -> Event {
    Event::Agent(AgentResult::Questions(vec![Question {
        text: "Which board?".into(),
    }]))
}

fn setup(
    respond: impl Fn(&AgentRequest) -> Event + Send + Sync + 'static,
) -> (Arc<Store>, Arc<FakeExecutor>, Arc<Scheduler<FakeExecutor>>) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let executor = FakeExecutor::new(respond);
    let projects: Projects = [(
        "maestro".to_owned(),
        Project {
            repo: "Badger-Embedded/maestro".into(),
            base: "main".into(),
            checks: Some("just lint".into()),
            models: factory_server::StageModels::default(),
            auto_merge: false,
            llm_review: false,
        },
    )]
    .into_iter()
    .collect();
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        Arc::new(projects),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
        mcp_runtime(&store).0,
    );
    (store, executor, scheduler)
}

/// An [`McpRuntime`] with its own token registry, an unavailable secret store
/// (no master key, so a keyless server still runs), and a proxy URL a test can
/// look for in a run's MCP config. The registry is returned too, so a test can
/// check a run's token is issued while it runs and revoked after.
fn mcp_runtime(store: &Arc<Store>) -> (McpRuntime, Arc<RunTokens>) {
    let tokens = Arc::new(RunTokens::new());
    let secrets: Arc<dyn SecretStore> = Arc::new(UnavailableSecrets::new(Arc::clone(store)));
    let runtime = McpRuntime {
        tokens: Arc::clone(&tokens),
        secrets,
        proxy_url: "http://factory:8081".into(),
        warnings: Arc::new(McpWarnings::new(Arc::clone(store))),
    };
    (runtime, tokens)
}

/// Like [`setup`], and also returns the MCP token registry the scheduler issues
/// run tokens into, for the tests that check the MCP config and its token.
fn setup_mcp(
    respond: impl Fn(&AgentRequest) -> Event + Send + Sync + 'static,
) -> (
    Arc<Store>,
    Arc<FakeExecutor>,
    Arc<Scheduler<FakeExecutor>>,
    Arc<RunTokens>,
) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let executor = FakeExecutor::new(respond);
    let projects: Projects = [(
        "maestro".to_owned(),
        Project {
            repo: "Badger-Embedded/maestro".into(),
            base: "main".into(),
            checks: Some("just lint".into()),
            models: factory_server::StageModels::default(),
            auto_merge: false,
            llm_review: false,
        },
    )]
    .into_iter()
    .collect();
    let (runtime, tokens) = mcp_runtime(&store);
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        Arc::new(projects),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
        runtime,
    );
    (store, executor, scheduler, tokens)
}

async fn wait_idle(scheduler: &Scheduler<FakeExecutor>) {
    for _ in 0..200 {
        if scheduler.running().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("runs still going: {:?}", scheduler.running());
}

async fn wait_for_requests(executor: &FakeExecutor, count: usize) {
    for _ in 0..200 {
        if executor.requests().len() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {count} requests, got {:?}", executor.requests());
}

#[tokio::test]
async fn queued_jobs_wait_for_a_free_worker() {
    let (store, executor, scheduler) = setup(|_| questions());
    let ids: Vec<i64> = (0..3)
        .map(|i| {
            store
                .create("maestro", "alice", &format!("request {i}"))
                .unwrap()
        })
        .collect();

    scheduler.tick().await.unwrap();
    assert_eq!(scheduler.running(), ids[..2]);
    assert_eq!(store.get(ids[2]).unwrap().job.stage(), &Stage::Requested);

    executor.release(2);
    wait_idle(&scheduler).await;
    for id in &ids[..2] {
        assert!(matches!(store.get(*id).unwrap().job.turn(), Turn::User(_)));
    }

    scheduler.tick().await.unwrap();
    assert_eq!(scheduler.running(), [ids[2]]);
    executor.release(1);
    wait_idle(&scheduler).await;
}

#[tokio::test]
async fn worker_runs_hold_numbered_slots_until_they_end() {
    let (store, executor, scheduler) = setup(|_| questions());
    let ids: Vec<i64> = (0..3)
        .map(|i| {
            store
                .create("maestro", "alice", &format!("request {i}"))
                .unwrap()
        })
        .collect();
    let slots = scheduler.slots();
    assert_eq!(slots.busy().len(), 0);

    scheduler.tick().await.unwrap();
    let busy: Vec<(usize, i64)> = slots
        .busy()
        .iter()
        .map(|slot| (slot.number, slot.job_id))
        .collect();
    assert_eq!(busy, [(1, ids[0]), (2, ids[1])]);

    executor.release(2);
    wait_idle(&scheduler).await;
    assert_eq!(slots.busy().len(), 0);

    scheduler.tick().await.unwrap();
    assert_eq!(slots.busy().len(), 1);
    assert_eq!(slots.busy()[0].number, 1);
    assert_eq!(slots.busy()[0].job_id, ids[2]);
    executor.release(1);
    wait_idle(&scheduler).await;
    assert_eq!(slots.busy().len(), 0);
}

#[tokio::test]
async fn cancelling_a_job_frees_its_slot() {
    let (store, executor, scheduler) = setup(|_| questions());
    let id = store.create("maestro", "alice", "request").unwrap();
    scheduler.tick().await.unwrap();
    assert_eq!(scheduler.slots().busy().len(), 1);

    store.apply(id, Event::Cancel).unwrap();
    scheduler.tick().await.unwrap();
    assert_eq!(scheduler.slots().busy().len(), 0);
    executor.release(1);
}

#[tokio::test]
async fn the_worker_limit_comes_from_the_stored_settings() {
    let (store, executor, scheduler) = setup(|_| questions());
    for i in 0..4 {
        store
            .create("maestro", "alice", &format!("request {i}"))
            .unwrap();
    }
    store
        .save_settings(&Settings {
            max_workers: 3,
            ..Settings::default()
        })
        .unwrap();

    scheduler.tick().await.unwrap();
    assert_eq!(scheduler.running().len(), 3);

    executor.release(4);
    wait_idle(&scheduler).await;
}

#[tokio::test]
async fn a_run_result_moves_the_job_and_is_logged() {
    let (store, executor, scheduler) = setup(|_| questions());
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();

    scheduler.tick().await.unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    let request = &executor.requests()[0];
    assert_eq!(request.job_id, id);
    assert_eq!(request.expect, Expect::Clarify);
    assert!(request.prompt.contains("Add a reboot button"));
    assert!(matches!(store.get(id).unwrap().job.turn(), Turn::User(_)));

    let texts: Vec<String> = store
        .logs(id, 10)
        .unwrap()
        .into_iter()
        .map(|entry| format!("{}: {}", entry.kind, entry.text))
        .collect();
    assert_eq!(
        texts,
        [
            format!(
                "factory: Agent run started in session {}.",
                request.session_id
            ),
            "text: Reading the repository.".to_owned(),
            "factory: Agent run finished, $0.12 at API prices.".to_owned(),
        ]
    );
}

#[tokio::test]
async fn a_project_onboarded_while_running_is_picked_up() {
    // The scheduler reads projects from the store, so one onboarded after it
    // starts is visible without a restart.
    let store = Arc::new(Store::open_in_memory().unwrap());
    let executor = FakeExecutor::new(|_| questions());
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        Arc::new(Projects::from_store(Arc::clone(&store))),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
        mcp_runtime(&store).0,
    );

    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    // Onboard the project while the scheduler is up.
    store
        .create_project(
            "maestro",
            "Badger-Embedded/maestro",
            "main",
            Some("just lint"),
            Some("alice"),
        )
        .unwrap();

    scheduler.tick().await.unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    // The job ran against the new project instead of failing as unconfigured.
    assert_eq!(executor.requests()[0].job_id, id);
    assert!(matches!(store.get(id).unwrap().job.turn(), Turn::User(_)));
}

#[tokio::test]
async fn answers_continue_the_session_and_each_later_stage_gets_its_own() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Clarify if request.prompt.contains("answered") => {
            Event::Agent(AgentResult::Clarified)
        }
        Expect::Clarify => questions(),
        Expect::Spec => Event::Agent(AgentResult::Spec("spec".into())),
        Expect::Plan => Event::Agent(AgentResult::Plan(vec![factory_core::PlannedPr {
            title: "a: b".into(),
            summary: "c".into(),
            number: None,
            description: None,
            notes: None,
        }])),
        Expect::Commits | Expect::FixCi => Event::Agent(AgentResult::CommitsReady(ready())),
        Expect::Review => unreachable!("this job keeps llm review off, so no reviewer runs"),
        Expect::FailureAnalysis | Expect::DiffCheck => unreachable!("this job is no Renovate job"),
    });
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    executor.release(10);

    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    for _ in 0..2 {
        scheduler.tick().await.unwrap();
        wait_idle(&scheduler).await;
    }
    store.apply(id, Event::Approve).unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;

    let requests = executor.requests();
    let expects: Vec<Expect> = requests.iter().map(|request| request.expect).collect();
    assert_eq!(
        expects,
        [
            Expect::Clarify,
            Expect::Clarify,
            Expect::Spec,
            Expect::Plan,
            Expect::Commits
        ]
    );
    assert!(requests[1].prompt.contains("A: rpi5"));
    assert!(
        requests
            .iter()
            .all(|r| r.repo == "Badger-Embedded/maestro" && r.base == "main")
    );
    assert!(requests[..4].iter().all(|r| r.branch.is_none()));
    assert_eq!(
        requests[4].branch.as_deref(),
        Some(format!("factory/job-{id}-pr-1").as_str())
    );
    assert_eq!(requests[0].session_id, requests[1].session_id);
    let ids: std::collections::HashSet<&str> =
        requests.iter().map(|r| r.session_id.as_str()).collect();
    assert_eq!(
        ids.len(),
        4,
        "clarify, spec, plan and the pull request each have a session"
    );
    assert!(requests[2].prompt.contains("Add a reboot button"));
    assert!(requests[3].prompt.contains("The approved spec:"));
    assert!(requests[4].prompt.contains("a: b"));
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Verifying { pr: 0 }
    );
}

#[tokio::test]
async fn cancelling_a_job_stops_its_run() {
    let (store, executor, scheduler) = setup(|_| questions());
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;

    store.apply(id, Event::Cancel).unwrap();
    scheduler.tick().await.unwrap();

    assert_eq!(scheduler.running(), Vec::<i64>::new());
    assert_eq!(*executor.stopped.lock().unwrap(), [id]);
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::Cancelled);
    let last = store.logs(id, 1).unwrap().pop().unwrap();
    assert!(
        last.text.contains("Stopped the running step"),
        "{}",
        last.text
    );
}

#[tokio::test]
async fn removing_a_project_stops_the_running_job() {
    let (store, executor, scheduler) = setup(|_| questions());
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;

    store.remove_project("maestro").unwrap();
    scheduler.tick().await.unwrap();

    assert_eq!(scheduler.running(), Vec::<i64>::new());
    assert_eq!(*executor.stopped.lock().unwrap(), [id]);
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::Cancelled);
    scheduler.tick().await.unwrap();
}

#[tokio::test]
async fn a_run_that_ends_after_the_project_was_removed_records_no_cost() {
    let (store, executor, scheduler) = setup(|_| questions());
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;

    store.remove_project("maestro").unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    let costs = store
        .events(id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event == "cost")
        .count();
    assert_eq!(costs, 0);
    assert_eq!(store.project_totals("maestro").unwrap().usd, 0.0);
}

#[tokio::test]
async fn a_job_has_a_task_until_its_stop_has_finished() {
    let (store, executor, scheduler) = setup(|_| questions());
    let slots = scheduler.slots();
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;
    assert!(slots.has_task(id));

    executor.hold_stops();
    store.remove_project("maestro").unwrap();
    let ticking = tokio::spawn({
        let scheduler = Arc::clone(&scheduler);
        async move { scheduler.tick().await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(slots.busy().len(), 0);
    assert!(slots.has_task(id));
    assert!(executor.stopped.lock().unwrap().is_empty());

    executor.stop_gate.add_permits(1);
    ticking.await.unwrap();
    assert!(!slots.has_task(id));
    assert_eq!(*executor.stopped.lock().unwrap(), [id]);
}

#[tokio::test]
async fn a_factory_task_counts_as_a_task_for_its_job() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    let slots = scheduler.slots();
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;

    executor.hold_watches();
    scheduler.tick().await.unwrap();
    assert_eq!(slots.busy().len(), 0);
    assert!(slots.has_task(id));

    executor.watch_gate.add_permits(1);
    wait_idle(&scheduler).await;
    assert!(!slots.has_task(id));
}

#[tokio::test]
async fn the_pass_lock_is_held_while_a_stop_is_in_progress() {
    let (store, executor, scheduler) = setup(|_| questions());
    let slots = scheduler.slots();
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;

    executor.hold_stops();
    store.remove_project("maestro").unwrap();
    let ticking = tokio::spawn({
        let scheduler = Arc::clone(&scheduler);
        async move { scheduler.tick().await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let waiting = tokio::time::timeout(Duration::from_millis(50), slots.lock_pass()).await;
    assert!(waiting.is_err());

    executor.stop_gate.add_permits(1);
    ticking.await.unwrap();
    let _pass = slots.lock_pass().await;
    assert!(!slots.has_task(id));
}

#[tokio::test]
async fn a_pass_that_read_a_job_before_its_removal_does_not_start_it() {
    let (store, executor, scheduler) = setup(|_| questions());
    let slots = scheduler.slots();
    let running = store.create("maestro", "alice", "first").unwrap();
    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;
    store.apply(running, Event::Cancel).unwrap();
    let queued = store.create("maestro", "alice", "second").unwrap();

    executor.hold_stops();
    let ticking = tokio::spawn({
        let scheduler = Arc::clone(&scheduler);
        async move { scheduler.tick().await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    store.remove_project("maestro").unwrap();
    let cleanup = tokio::time::timeout(Duration::from_millis(50), slots.lock_pass()).await;
    assert!(cleanup.is_err());

    executor.stop_gate.add_permits(1);
    ticking.await.unwrap();
    let _pass = slots.lock_pass().await;
    assert_eq!(executor.requests().len(), 1);
    assert!(!slots.has_task(queued));
    assert_eq!(store.get(queued).unwrap().job.stage(), &Stage::Cancelled);
}

#[tokio::test]
async fn an_earlier_task_cannot_remove_the_entry_of_its_replacement() {
    let slots = WorkerSlots::new();
    let mut ids = Vec::new();
    for _ in 0..2 {
        slots.register_task(7, false, |id| {
            ids.push(id);
            tokio::spawn(std::future::pending())
        });
    }

    slots.finish_task(7, ids[0]);
    assert!(slots.has_task(7));

    slots.finish_task(7, ids[1]);
    assert!(!slots.has_task(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_registered_while_the_previous_task_ends_keeps_its_entry() {
    let slots = Arc::new(WorkerSlots::new());
    let (go, go_signal) = std::sync::mpsc::channel::<()>();
    let (done, done_signal) = std::sync::mpsc::channel::<()>();

    slots.register_task(7, true, |id| {
        let slots = Arc::clone(&slots);
        std::thread::spawn(move || {
            go_signal.recv().unwrap();
            slots.finish_task(7, id);
            done.send(()).unwrap();
        });
        tokio::spawn(std::future::pending())
    });

    slots.register_task(7, true, |_| {
        go.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while slots.finishes_that_waited() == 0 {
            assert!(
                Instant::now() < deadline,
                "the earlier task never waited for the registry"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        tokio::spawn(std::future::pending())
    });
    done_signal.recv_timeout(Duration::from_secs(5)).unwrap();

    assert!(slots.has_task(7));
    assert_eq!(slots.running(), [7]);
}

#[tokio::test]
async fn a_failed_run_fails_the_job() {
    let (store, executor, scheduler) = setup(|_| Event::AgentFailed {
        reason: "the worker exited (exit status: 125) without a result".into(),
    });
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    assert!(matches!(
        store.get(id).unwrap().job.stage(),
        Stage::Failed { reason, .. } if reason.contains("125")
    ));
    scheduler.tick().await.unwrap();
    assert_eq!(scheduler.running(), Vec::<i64>::new());
    assert_eq!(executor.requests().len(), 1);
}

#[tokio::test]
async fn a_job_waiting_for_the_user_takes_no_worker() {
    let (store, executor, scheduler) = setup(|_| questions());
    let waiting = store.create("maestro", "alice", "first").unwrap();
    scheduler.tick().await.unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    store
        .save_settings(&Settings {
            max_workers: 1,
            ..Settings::default()
        })
        .unwrap();
    let next = store.create("maestro", "alice", "second").unwrap();
    scheduler.tick().await.unwrap();
    assert_eq!(scheduler.running(), [next]);
    assert!(matches!(
        store.get(waiting).unwrap().job.turn(),
        Turn::User(_)
    ));
    executor.release(1);
    wait_idle(&scheduler).await;
}

#[tokio::test]
async fn a_job_for_an_unknown_project_fails() {
    let (store, executor, scheduler) = setup(|_| questions());
    let id = store.create("gone", "alice", "x").unwrap();
    scheduler.tick().await.unwrap();

    assert_eq!(executor.requests().len(), 0);
    assert!(matches!(
        store.get(id).unwrap().job.stage(),
        Stage::Failed { reason, .. } if reason.contains("\"gone\"")
    ));
}

/// Runs a clarify run that asks a question, answers it, and runs again, with
/// the executor reporting `costs` for the two runs.
async fn two_clarify_runs(
    costs: [Option<f64>; 2],
) -> (Arc<Store>, Arc<Scheduler<FakeExecutor>>, i64) {
    let (store, executor, scheduler) = setup(|request| {
        if request.prompt.contains("A: rpi5") {
            Event::Agent(AgentResult::Clarified)
        } else {
            questions()
        }
    });
    executor.costs.lock().unwrap().extend(costs);
    executor.release(10);
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;
    (store, scheduler, id)
}

#[tokio::test]
async fn a_resumed_run_adds_what_the_session_cost_since_its_last_run() {
    // Claude reports the session's total, which keeps counting across
    // --resume, so the second run cost 0.75, not 1.25.
    let (store, _scheduler, id) = two_clarify_runs([Some(0.5), Some(1.25)]).await;

    assert!((store.cost_usd(id).unwrap() - 1.25).abs() < 1e-9);
    let finished: Vec<String> = store
        .logs(id, 20)
        .unwrap()
        .into_iter()
        .map(|entry| entry.text)
        .filter(|text| text.starts_with("Agent run finished"))
        .collect();
    assert_eq!(
        finished,
        [
            format!("Agent run finished, {} at API prices.", format_cost(0.5)),
            format!(
                "Agent run finished, {} at API prices, {} for the session so far.",
                format_cost(0.75),
                format_cost(1.25)
            ),
        ]
    );
}

#[tokio::test]
async fn a_session_total_that_drops_counts_as_a_new_session() {
    // A total below the stored one means the agent started the session over,
    // so the whole total is the run's cost.
    let (store, _scheduler, id) = two_clarify_runs([Some(0.5), Some(0.25)]).await;

    assert!((store.cost_usd(id).unwrap() - 0.75).abs() < 1e-9);
}

#[tokio::test]
async fn a_run_without_a_cost_leaves_the_total_alone() {
    let (store, _scheduler, id) = two_clarify_runs([Some(0.5), None]).await;

    assert!((store.cost_usd(id).unwrap() - 0.5).abs() < 1e-9);
    let finished: Vec<String> = store
        .logs(id, 20)
        .unwrap()
        .into_iter()
        .map(|entry| entry.text)
        .filter(|text| text.starts_with("Agent run finished"))
        .collect();
    assert_eq!(
        finished,
        [
            format!("Agent run finished, {} at API prices.", format_cost(0.5)),
            "Agent run finished.".to_owned()
        ]
    );
}

#[tokio::test]
async fn a_job_that_never_ran_costs_nothing() {
    let (store, _executor, _scheduler) = setup(|_| questions());
    let id = store.create("maestro", "alice", "x").unwrap();

    assert_eq!(store.cost_usd(id).unwrap(), 0.0);
}

#[tokio::test]
async fn a_cancelled_job_keeps_its_cost() {
    let (store, _scheduler, id) = two_clarify_runs([Some(0.5), Some(0.25)]).await;
    store.apply(id, Event::Cancel).unwrap();

    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::Cancelled);
    assert!((store.cost_usd(id).unwrap() - 0.75).abs() < 1e-9);
}

#[tokio::test]
async fn a_failed_job_keeps_its_cost() {
    let (store, executor, scheduler) = setup(|_| Event::AgentFailed {
        reason: "the worker exited without a result".into(),
    });
    executor.costs.lock().unwrap().push_back(Some(0.4));
    executor.release(1);
    let id = store.create("maestro", "alice", "x").unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;

    assert!(matches!(
        store.get(id).unwrap().job.stage(),
        Stage::Failed { .. }
    ));
    assert!((store.cost_usd(id).unwrap() - 0.4).abs() < 1e-9);
}

#[test]
fn cost_formats_as_dollars_and_cents() {
    assert_eq!(format_cost(1.234), "$1.23");
    assert_eq!(format_cost(0.0), "$0.00");
    assert_eq!(format_cost(12.0), "$12.00");
}

/// A sample Codex price row.
fn codex_price(model: &str) -> CodexPrice {
    CodexPrice {
        model: model.to_owned(),
        input_usd_per_mtok: 2.0,
        cached_input_usd_per_mtok: 0.5,
        output_usd_per_mtok: 8.0,
    }
}

/// A sample Codex run's summed token usage.
fn codex_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 1000,
        cached_input_tokens: 200,
        output_tokens: 500,
        reasoning_output_tokens: 120,
    }
}

/// Sets the project's clarify stage to Codex with `model`, saves `price` when
/// given, queues `usage` for the run, and runs one clarify run on Codex.
async fn codex_clarify_run(
    model: Option<&str>,
    price: Option<CodexPrice>,
    usage: TokenUsage,
) -> (Arc<Store>, i64) {
    let (store, executor, scheduler) = setup(|_| questions());
    store
        .save_project_agents(
            "maestro",
            &StageAgents {
                clarify: "codex".into(),
                ..StageAgents::default()
            },
        )
        .unwrap();
    if let Some(model) = model {
        store
            .save_project_models(
                "maestro",
                &StageModels {
                    clarify: Some(model.to_owned()),
                    ..StageModels::default()
                },
            )
            .unwrap();
    }
    if let Some(price) = price {
        store.set_codex_price(&price).unwrap();
    }
    executor.token_usages.lock().unwrap().push_back(usage);
    executor.release(1);
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;
    (store, id)
}

/// The job log lines that report a finished run.
fn finished_lines(store: &Store, id: i64) -> Vec<String> {
    store
        .logs(id, 20)
        .unwrap()
        .into_iter()
        .map(|entry| entry.text)
        .filter(|text| text.contains("run finished"))
        .collect()
}

#[tokio::test]
async fn a_codex_run_cost_is_estimated_from_the_price_table() {
    let (store, id) = codex_clarify_run(
        Some("gpt-6.1-sol"),
        Some(codex_price("gpt-6.1-sol")),
        codex_usage(),
    )
    .await;

    // 800 uncached input at $2, 200 cached at $0.50, 500 output at $8, per Mtok.
    let expected = 800.0 / 1e6 * 2.0 + 200.0 / 1e6 * 0.5 + 500.0 / 1e6 * 8.0;
    assert!((store.cost_usd(id).unwrap() - expected).abs() < 1e-12);
    // The row is marked estimated, so the job page flags the total.
    assert!(store.has_estimated_cost(id).unwrap());
    let finished = finished_lines(&store, id);
    assert_eq!(finished.len(), 1);
    assert!(finished[0].contains("estimated"));
    assert!(finished[0].contains("gpt-6.1-sol"));
    assert!(finished[0].contains("1000 input tokens (200 cached), 500 output tokens"));
}

#[tokio::test]
async fn a_codex_run_with_no_price_for_the_model_adds_no_cost() {
    let (store, id) = codex_clarify_run(Some("gpt-6.1-sol"), None, codex_usage()).await;

    assert_eq!(store.cost_usd(id).unwrap(), 0.0);
    assert!(!store.has_estimated_cost(id).unwrap());
    let finished = finished_lines(&store, id);
    assert_eq!(finished.len(), 1);
    assert!(finished[0].contains("No price is set for the model gpt-6.1-sol"));
    // The token counts still go to the log.
    assert!(finished[0].contains("1000 input tokens (200 cached), 500 output tokens"));
}

#[tokio::test]
async fn a_codex_run_with_no_model_adds_no_cost() {
    let (store, id) =
        codex_clarify_run(None, Some(codex_price("gpt-6.1-sol")), codex_usage()).await;

    assert_eq!(store.cost_usd(id).unwrap(), 0.0);
    assert!(!store.has_estimated_cost(id).unwrap());
    let finished = finished_lines(&store, id);
    assert_eq!(finished.len(), 1);
    assert!(finished[0].contains("passed no model"));
    assert!(finished[0].contains("1000 input tokens (200 cached), 500 output tokens"));
}

#[tokio::test]
async fn a_copilot_run_adds_no_cost_and_the_log_says_so_once() {
    let (store, executor, scheduler) = setup(|_| questions());
    store
        .save_project_agents(
            "maestro",
            &StageAgents {
                clarify: "copilot".into(),
                ..StageAgents::default()
            },
        )
        .unwrap();
    executor.release(1);
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;

    assert_eq!(executor.requests()[0].agent, Agent::Copilot);
    assert_eq!(store.cost_usd(id).unwrap(), 0.0);
    assert!(!store.has_estimated_cost(id).unwrap());
    assert_eq!(store.project_totals("maestro").unwrap().usd, 0.0);
    assert!(
        store
            .events(id)
            .unwrap()
            .iter()
            .all(|event| event.event != "cost"),
        "a Copilot run must not write a cost row",
    );
    let finished = finished_lines(&store, id);
    assert_eq!(finished.len(), 1);
    assert!(finished[0].contains("Copilot runs are not counted in the cost"));
}

fn planned_pr(title: &str) -> factory_core::PlannedPr {
    factory_core::PlannedPr {
        title: title.into(),
        summary: "s".into(),
        number: None,
        description: None,
        notes: None,
    }
}

#[test]
fn each_session_of_a_job_keeps_its_own_total() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    let (clarify, _) = store
        .session_uuid(id, factory_core::Session::Planning, Agent::Claude)
        .unwrap();
    let (implement, _) = store
        .session_uuid(id, factory_core::Session::Pr(0), Agent::Claude)
        .unwrap();

    assert!((store.add_session_cost(id, &clarify, 1.0).unwrap() - 1.0).abs() < 1e-9);
    // A different session starts its own total, so nothing is subtracted.
    assert!((store.add_session_cost(id, &implement, 3.0).unwrap() - 3.0).abs() < 1e-9);
    assert!((store.add_session_cost(id, &implement, 3.5).unwrap() - 0.5).abs() < 1e-9);
    assert!((store.add_session_cost(id, &clarify, 1.25).unwrap() - 0.25).abs() < 1e-9);
    assert!((store.cost_usd(id).unwrap() - 4.75).abs() < 1e-9);
}

#[test]
fn totals_count_created_jobs_per_project() {
    let store = Store::open_in_memory().unwrap();
    store.create("maestro", "alice", "x").unwrap();
    store.create("maestro", "bob", "y").unwrap();
    store.create("factory", "alice", "z").unwrap();

    let maestro = store.project_totals("maestro").unwrap();
    assert_eq!((maestro.jobs_created, maestro.jobs_merged), (2, 0));
    assert_eq!(store.project_totals("factory").unwrap().jobs_created, 1);
    let unknown = store.project_totals("nobody").unwrap();
    assert_eq!(
        (unknown.usd, unknown.jobs_created, unknown.jobs_merged),
        (0.0, 0, 0)
    );
}

#[test]
fn totals_sum_the_cost_of_every_run_in_the_project() {
    let store = Store::open_in_memory().unwrap();
    let first = store.create("maestro", "alice", "x").unwrap();
    let second = store.create("maestro", "alice", "y").unwrap();
    let other = store.create("factory", "alice", "z").unwrap();
    store.add_cost(first, 0.5, false).unwrap();
    store.add_cost(first, 0.25, false).unwrap();
    store.add_cost(second, 1.0, false).unwrap();
    store.add_cost(other, 4.0, false).unwrap();

    assert!((store.project_totals("maestro").unwrap().usd - 1.75).abs() < 1e-9);
    assert!((store.project_totals("factory").unwrap().usd - 4.0).abs() < 1e-9);
}

#[test]
fn a_job_with_two_pull_requests_counts_as_merged_once() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    for event in [
        Event::Start,
        Event::Agent(AgentResult::Clarified),
        Event::Agent(AgentResult::Spec("s".into())),
        Event::Approve,
        Event::Agent(AgentResult::Plan(vec![planned_pr("a"), planned_pr("b")])),
        Event::Approve,
        Event::Agent(AgentResult::CommitsReady(ready())),
        Event::ChecksPassed,
        Event::Published { number: 3 },
        Event::Merged,
    ] {
        store.apply(id, event).unwrap();
    }
    assert_ne!(store.get(id).unwrap().job.stage(), &Stage::Merged);
    assert_eq!(store.project_totals("maestro").unwrap().jobs_merged, 0);

    for event in [
        Event::Agent(AgentResult::CommitsReady(ready())),
        Event::ChecksPassed,
        Event::Published { number: 4 },
        Event::Merged,
    ] {
        store.apply(id, event).unwrap();
    }
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::Merged);
    assert_eq!(store.project_totals("maestro").unwrap().jobs_merged, 1);

    assert!(store.apply(id, Event::Merged).is_err());
    assert_eq!(store.project_totals("maestro").unwrap().jobs_merged, 1);
}

fn history(store: &Store, id: i64) -> Vec<(String, String, Option<usize>, String)> {
    store
        .events(id)
        .unwrap()
        .into_iter()
        .map(|e| (e.event, e.stage, e.pr, e.turn))
        .collect()
}

fn entry(
    event: &str,
    stage: &str,
    pr: Option<usize>,
    turn: &str,
) -> (String, String, Option<usize>, String) {
    (event.into(), stage.into(), pr, turn.into())
}

#[test]
fn the_history_records_each_event_with_the_stage_and_turn_it_left() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    for event in [
        Event::Start,
        Event::Agent(AgentResult::Clarified),
        Event::Agent(AgentResult::Spec("s".into())),
        Event::Approve,
        Event::Agent(AgentResult::Plan(vec![planned_pr("a"), planned_pr("b")])),
        Event::Approve,
        Event::ChecksFailed { report: "r".into() },
    ] {
        let _ = store.apply(id, event);
    }
    assert_eq!(
        history(&store, id),
        vec![
            entry("created", "requested", None, "factory"),
            entry("start", "clarifying", None, "agent:clarify"),
            entry("agent:clarified", "spec", None, "agent:write-spec"),
            entry("agent:spec", "spec", None, "user:spec-approval"),
            entry("approve", "plan", None, "agent:write-plan"),
            entry("agent:plan", "plan", None, "user:plan-approval"),
            entry("approve", "implementing", Some(0), "agent:implement"),
        ],
        "a rejected event adds nothing"
    );
    for entry in store.events(id).unwrap() {
        assert_eq!(entry.at.len(), "2026-11-20 12:01:38".len());
    }
}

#[test]
fn the_history_keeps_failure_reasons_and_run_costs() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create("maestro", "alice", "x").unwrap();
    store.apply(id, Event::Start).unwrap();
    store.add_cost(id, 0.75, false).unwrap();
    store
        .apply(
            id,
            Event::AgentFailed {
                reason: "usage limit".into(),
            },
        )
        .unwrap();

    let events = store.events(id).unwrap();
    let cost = &events[2];
    assert_eq!(
        (cost.event.as_str(), cost.stage.as_str()),
        ("cost", "clarifying")
    );
    assert_eq!(cost.usd, Some(0.75));
    let failed = &events[3];
    assert_eq!(
        (failed.event.as_str(), failed.stage.as_str()),
        ("agent-failed", "failed")
    );
    assert_eq!(failed.detail.as_deref(), Some("usage limit"));
    assert_eq!(failed.usd, None);
}

#[test]
fn a_job_with_no_history_reads_an_empty_one() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.events(42).unwrap(), Vec::new());
}

#[test]
fn cancelled_and_failed_jobs_count_as_created_only() {
    let store = Store::open_in_memory().unwrap();
    let cancelled = store.create("maestro", "alice", "x").unwrap();
    let failed = store.create("maestro", "alice", "y").unwrap();
    store.apply(cancelled, Event::Cancel).unwrap();
    store.apply(failed, Event::Start).unwrap();
    store
        .apply(failed, Event::AgentFailed { reason: "x".into() })
        .unwrap();

    let totals = store.project_totals("maestro").unwrap();
    assert_eq!((totals.jobs_created, totals.jobs_merged), (2, 0));
}

/// Runs the job until its first pull request's commits are ready.
async fn to_commits_ready(
    store: &Store,
    executor: &FakeExecutor,
    scheduler: &Arc<Scheduler<FakeExecutor>>,
) -> i64 {
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    store.apply(id, Event::Start).unwrap();
    store
        .apply(id, Event::Agent(AgentResult::Clarified))
        .unwrap();
    store
        .apply(id, Event::Agent(AgentResult::Spec("spec".into())))
        .unwrap();
    store.apply(id, Event::Approve).unwrap();
    store
        .apply(
            id,
            Event::Agent(AgentResult::Plan(vec![
                factory_core::PlannedPr {
                    title: "maestrod: add a reboot endpoint".into(),
                    summary: "POST /reboot".into(),
                    number: None,
                    description: None,
                    notes: None,
                },
                factory_core::PlannedPr {
                    title: "web: add a reboot button".into(),
                    summary: "Calls it.".into(),
                    number: None,
                    description: None,
                    notes: None,
                },
            ])),
        )
        .unwrap();
    store.apply(id, Event::Approve).unwrap();
    executor.release(1);
    scheduler.tick().await.unwrap();
    wait_idle(scheduler).await;
    id
}

fn quick_polls(store: &Store) {
    store
        .save_settings(&Settings {
            poll_seconds: 0,
            ..Settings::default()
        })
        .unwrap();
}

async fn step(scheduler: &Arc<Scheduler<FakeExecutor>>) {
    scheduler.tick().await.unwrap();
    wait_idle(scheduler).await;
}

#[tokio::test]
async fn commits_go_through_checks_and_publish_to_a_watched_pull_request() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Verifying { pr: 0 }
    );

    step(&scheduler).await;
    assert_eq!(executor.checks_run.lock().unwrap()[0].command, "just lint");
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::PrOpen { pr: 0 });

    step(&scheduler).await;
    let published = executor.published.lock().unwrap()[0].clone();
    assert_eq!(published.branch, format!("factory/job-{id}-pr-1"));
    assert_eq!(published.description, ready());
    assert_eq!(published.repo, "Badger-Embedded/maestro");
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::CiAndReview { pr: 0 }
    );

    executor.watches.lock().unwrap().push_back(Ok(WatchReport {
        event: Some(Event::Merged),
        seen_until: None,
        merged_by_factory: false,
    }));
    step(&scheduler).await;
    assert_eq!(executor.watched.lock().unwrap()[0].number, 12);
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Implementing { pr: 1 }
    );
}

#[tokio::test]
async fn failed_checks_go_back_to_the_agent() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    executor
        .checks
        .lock()
        .unwrap()
        .push_back(Ok(Event::ChecksFailed {
            report: "clippy: unused variable".into(),
        }));

    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::Agent(factory_core::Task::FixChecks { report }) if report.contains("clippy")
    ));
}

#[tokio::test]
async fn a_project_without_checks_publishes_unchecked() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    let id = store.create("bare", "alice", "x").unwrap();
    let projects: Projects = [(
        "bare".to_owned(),
        Project {
            repo: "o/bare".into(),
            base: "main".into(),
            checks: None,
            models: factory_server::StageModels::default(),
            auto_merge: false,
            llm_review: false,
        },
    )]
    .into_iter()
    .collect();
    let scheduler2 = Scheduler::new(
        Arc::clone(&store),
        Arc::new(projects),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
        mcp_runtime(&store).0,
    );
    drop(scheduler);
    for event in [
        Event::Start,
        Event::Agent(AgentResult::Clarified),
        Event::Agent(AgentResult::Spec("s".into())),
        Event::Approve,
        Event::Agent(AgentResult::Plan(vec![factory_core::PlannedPr {
            title: "t".into(),
            summary: "s".into(),
            number: None,
            description: None,
            notes: None,
        }])),
        Event::Approve,
        Event::Agent(AgentResult::CommitsReady(ready())),
    ] {
        store.apply(id, event).unwrap();
    }

    scheduler2.tick().await.unwrap();
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::PrOpen { pr: 0 });
    assert_eq!(executor.checks_run.lock().unwrap().len(), 0);
    let last = store.logs(id, 1).unwrap().pop().unwrap();
    assert!(last.text.contains("no checks"), "{}", last.text);
}

#[tokio::test]
async fn a_failed_publish_waits_before_it_is_tried_again() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    executor
        .publishes
        .lock()
        .unwrap()
        .push_back(Err("gh pr create failed: HTTP 502".into()));

    step(&scheduler).await;
    step(&scheduler).await;
    assert_eq!(executor.published.lock().unwrap().len(), 1);
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::PrOpen { pr: 0 });
    let last = store.logs(id, 1).unwrap().pop().unwrap();
    assert!(last.text.contains("HTTP 502"), "{}", last.text);
    assert!(
        last.text.contains("tries again in 60 seconds"),
        "{}",
        last.text
    );

    let fresh = Scheduler::new(
        Arc::clone(&store),
        Arc::new(
            [(
                "maestro".to_owned(),
                Project {
                    repo: "Badger-Embedded/maestro".into(),
                    base: "main".into(),
                    checks: Some("just lint".into()),
                    models: factory_server::StageModels::default(),
                    auto_merge: false,
                    llm_review: false,
                },
            )]
            .into_iter()
            .collect(),
        ),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
        mcp_runtime(&store).0,
    );
    step(&fresh).await;
    assert_eq!(executor.published.lock().unwrap().len(), 2);
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::CiAndReview { pr: 0 }
    );
}

#[tokio::test]
async fn a_pull_request_merged_before_the_push_moves_the_job_on() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::PrOpen { pr: 0 });
    executor
        .publishes
        .lock()
        .unwrap()
        .push_back(Ok(Published::AlreadyMerged(12)));

    step(&scheduler).await;
    let job = store.get(id).unwrap().job;
    assert_eq!(job.stage(), &Stage::Implementing { pr: 1 });
    assert_eq!(job.plan()[0].number, Some(12));
    let logs = store.logs(id, 10).unwrap();
    assert!(
        logs.iter()
            .any(|line| line.text.contains("#12 was merged before the push")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn a_branch_with_nothing_to_publish_skips_the_pull_request() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    assert_eq!(store.get(id).unwrap().job.stage(), &Stage::PrOpen { pr: 0 });
    executor
        .publishes
        .lock()
        .unwrap()
        .push_back(Ok(Published::Empty));

    step(&scheduler).await;
    let job = store.get(id).unwrap().job;
    assert_eq!(job.stage(), &Stage::Implementing { pr: 1 });
    assert!(job.is_skipped(0));
    assert_eq!(job.plan()[0].number, None);
    let logs = store.logs(id, 10).unwrap();
    assert!(
        logs.iter()
            .any(|line| line.text.contains("nothing to publish")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn review_comments_reach_the_agent_once() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    executor.watches.lock().unwrap().push_back(Ok(WatchReport {
        event: Some(Event::ReviewComments {
            comments: "talhahavadar:\nNeeds a test.".into(),
        }),
        seen_until: Some("2026-11-19T12:05:00Z".into()),
        merged_by_factory: false,
    }));

    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::Agent(factory_core::Task::AddressReview { .. })
    ));
    assert_eq!(
        store.pr_watermark(id, 0).unwrap().as_deref(),
        Some("2026-11-19T12:05:00Z")
    );

    executor.release(1);
    for _ in 0..4 {
        step(&scheduler).await;
    }
    let watched = executor.watched.lock().unwrap().clone();
    assert_eq!(watched[0].seen_until, None);
    assert_eq!(
        watched.last().unwrap().seen_until.as_deref(),
        Some("2026-11-19T12:05:00Z")
    );
}

#[tokio::test]
async fn an_open_pull_request_is_watched_once_per_interval() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    let _id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    store.save_settings(&Settings::default()).unwrap();

    step(&scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    assert_eq!(executor.watched.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn auto_merge_reaches_the_executor_and_a_factory_merge_is_logged() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    store
        .save_settings(&Settings {
            poll_seconds: 0,
            ..Settings::default()
        })
        .unwrap();
    store.save_project_auto_merge("maestro", true).unwrap();
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    executor.watches.lock().unwrap().push_back(Ok(WatchReport {
        event: Some(Event::Merged),
        seen_until: None,
        merged_by_factory: true,
    }));

    step(&scheduler).await;
    assert!(executor.watched.lock().unwrap()[0].auto_merge);
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Implementing { pr: 1 }
    );
    let texts: Vec<String> = store
        .logs(id, 20)
        .unwrap()
        .into_iter()
        .map(|entry| entry.text)
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t == "Merged #12: its checks passed and auto-merge is on."),
        "{texts:?}"
    );
}

#[tokio::test]
async fn a_conflict_sends_the_agent_to_rebase_on_the_pull_request_branch() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    executor.watches.lock().unwrap().push_back(Ok(WatchReport {
        event: Some(Event::Conflicted {
            base: "main".into(),
        }),
        seen_until: None,
        merged_by_factory: false,
    }));

    step(&scheduler).await;
    assert_eq!(executor.watched.lock().unwrap()[0].base, "main");
    executor.release(1);
    step(&scheduler).await;

    let rebase = executor.requests().last().unwrap().clone();
    assert!(rebase.prompt.contains("origin/main"), "{}", rebase.prompt);
    assert_eq!(
        rebase.branch.as_deref(),
        Some(format!("factory/job-{id}-pr-1").as_str())
    );
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Verifying { pr: 0 }
    );
}

#[tokio::test]
async fn the_publish_gate_holds_the_push_until_the_user_approves() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    store
        .save_settings(&Settings {
            poll_seconds: 0,
            gates: Gates {
                publish: true,
                ..Gates::default()
            },
            ..Settings::default()
        })
        .unwrap();
    let id = to_commits_ready(&store, &executor, &scheduler).await;

    step(&scheduler).await;
    step(&scheduler).await;
    let summarized = executor.summarized.lock().unwrap().clone();
    assert_eq!(summarized.len(), 1);
    assert_eq!(summarized[0].job_id, id);
    assert_eq!(summarized[0].base, "main");
    let job = store.get(id).unwrap().job;
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::PublishApproval {
            summary: Some(BranchSummary {
                commits: vec!["abc1234 maestrod: add a reboot endpoint".into()],
                diff_stat: " src/main.rs | 4 ++++".into(),
            })
        })
    );

    step(&scheduler).await;
    step(&scheduler).await;
    assert!(executor.published.lock().unwrap().is_empty());
    assert_eq!(executor.summarized.lock().unwrap().len(), 1);

    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    assert_eq!(executor.published.lock().unwrap().len(), 1);
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::CiAndReview { pr: 0 }
    );
}

/// The project reviews before merge and squash-merges on approval.
fn reviewing() -> factory_server::StageModels {
    factory_server::StageModels {
        review: Some("review-model".into()),
        ..Default::default()
    }
}

/// Takes a job to a pull request the factory is watching in ci-and-review,
/// through the project's checks and a publish.
async fn to_watching(
    store: &Store,
    executor: &FakeExecutor,
    scheduler: &Arc<Scheduler<FakeExecutor>>,
) -> i64 {
    let id = to_commits_ready(store, executor, scheduler).await;
    step(scheduler).await; // checks pass -> pr-open
    step(scheduler).await; // publish -> ci-and-review
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::CiAndReview { pr: 0 }
    );
    id
}

/// Queues a watch that finds the pull request green, so the next watch
/// returns the event the factory decided on for a ready pull request.
fn green(executor: &FakeExecutor, event: Event) {
    executor.watches.lock().unwrap().push_back(Ok(WatchReport {
        event: Some(event),
        seen_until: None,
        merged_by_factory: false,
    }));
}

#[test]
fn a_ready_pull_request_reviews_before_it_merges_or_waits() {
    // Review off: auto-merge alone decides a merge or a wait, as before.
    assert_eq!(ready_action(false, false, true), ReadyAction::Merge);
    assert_eq!(ready_action(false, false, false), ReadyAction::Wait);
    // Review on and the head not yet approved: review first, whatever
    // auto-merge says.
    assert_eq!(ready_action(true, false, true), ReadyAction::Review);
    assert_eq!(ready_action(true, false, false), ReadyAction::Review);
    // Review on and the head approved: merge or wait like a project with no
    // review.
    assert_eq!(ready_action(true, true, true), ReadyAction::Merge);
    assert_eq!(ready_action(true, true, false), ReadyAction::Wait);
}

#[tokio::test]
async fn the_watch_carries_the_review_flag_and_the_head_review_state() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    store.save_project_settings("maestro", false, true).unwrap();
    to_watching(&store, &executor, &scheduler).await;

    step(&scheduler).await;
    let watched = executor.watched.lock().unwrap()[0].clone();
    assert!(watched.llm_review);
    assert!(!watched.review_approved);
}

#[tokio::test]
async fn an_approved_review_lets_an_auto_merge_go_through() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewApproved,
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", true, true).unwrap();
    store.save_project_models("maestro", &reviewing()).unwrap();
    let id = to_watching(&store, &executor, &scheduler).await;

    // CI is green: the factory starts the review instead of merging.
    green(&executor, start_review());
    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::Agent(factory_core::Task::Review)
    ));

    // The reviewer reads the branch in a session of its own, with the review
    // model, and does not merge.
    executor.release(1);
    step(&scheduler).await;
    let requests = executor.requests();
    let review = requests.last().unwrap();
    assert_eq!(review.expect, Expect::Review);
    assert_eq!(review.model.as_deref(), Some("review-model"));
    assert_eq!(
        review.branch.as_deref(),
        Some(format!("factory/job-{id}-pr-1").as_str())
    );
    assert_ne!(review.session_id, requests[0].session_id);
    let job = store.get(id).unwrap().job;
    assert_eq!(job.reviews().len(), 1);
    assert_eq!(job.reviews()[0].verdict, factory_core::Verdict::Approve);

    // The head is approved now, so the next watch merges.
    green(&executor, Event::Merged);
    step(&scheduler).await;
    assert!(
        executor
            .watched
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .review_approved
    );
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Implementing { pr: 1 }
    );
}

#[tokio::test]
async fn with_auto_merge_off_the_review_runs_and_the_factory_does_not_merge() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewApproved,
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", false, true).unwrap();
    let id = to_watching(&store, &executor, &scheduler).await;

    green(&executor, start_review());
    step(&scheduler).await;
    executor.release(1);
    step(&scheduler).await;
    assert_eq!(store.get(id).unwrap().job.reviews().len(), 1);

    // The head is approved and auto-merge is off. A plain watch (no event)
    // leaves the job watching, and the flags it carries tell the executor the
    // head is reviewed, so `ready_action` would wait rather than merge.
    step(&scheduler).await;
    let watched = executor.watched.lock().unwrap().last().unwrap().clone();
    assert!(watched.llm_review);
    assert!(watched.review_approved);
    assert!(!watched.auto_merge);
    assert_eq!(
        ready_action(
            watched.llm_review,
            watched.review_approved,
            watched.auto_merge
        ),
        ReadyAction::Wait
    );
    let job = store.get(id).unwrap().job;
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert!(matches!(job.turn(), Turn::Factory));
}

#[tokio::test]
async fn requested_changes_go_to_the_implement_session_and_review_again() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewChanges {
            feedback: "src/main.rs:10: handle the error".into(),
        },
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", true, true).unwrap();
    let id = to_watching(&store, &executor, &scheduler).await;

    green(&executor, start_review());
    step(&scheduler).await;
    executor.release(1);
    step(&scheduler).await; // review asks for changes

    let job = store.get(id).unwrap().job;
    assert_eq!(job.reviews()[0].verdict, factory_core::Verdict::Changes);
    assert!(matches!(
        job.turn(),
        Turn::Agent(factory_core::Task::AddressReview { comments })
            if comments.contains("handle the error")
    ));
    let review = executor.requests().last().unwrap().clone();
    let review_session = review.session_id.clone();

    // The fix runs in the implement session, not the review session, and its
    // prompt carries the feedback.
    executor.release(1);
    step(&scheduler).await;
    let fix = executor.requests().last().unwrap().clone();
    assert_eq!(fix.expect, Expect::Commits);
    assert_ne!(fix.session_id, review_session);
    assert!(fix.prompt.contains("handle the error"), "{}", fix.prompt);
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Verifying { pr: 0 }
    );

    // After the push the head is unapproved again, so a green pull request
    // starts another review.
    step(&scheduler).await; // checks pass -> pr-open
    step(&scheduler).await; // publish -> ci-and-review
    assert_eq!(
        store.get(id).unwrap().job.review_state(),
        ReviewState::Pending
    );
    green(&executor, start_review());
    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::Agent(factory_core::Task::Review)
    ));
}

#[tokio::test]
async fn a_reviewer_failure_stops_the_job_for_the_user_without_merging() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewFailed {
            reason: "the reviewer run ended without structured output".into(),
        },
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", true, true).unwrap();
    let id = to_watching(&store, &executor, &scheduler).await;

    green(&executor, start_review());
    step(&scheduler).await;
    executor.release(1);
    step(&scheduler).await; // reviewer fails

    let job = store.get(id).unwrap().job;
    assert!(matches!(
        job.turn(),
        Turn::User(Ask::ReviewStopped { reason, .. }) if reason.contains("structured output")
    ));
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });

    // The factory keeps watching the pull request, but the watch is passive:
    // it carries `watch_only`, so the executor reports only a merge or close
    // and never merges or restarts the reviewer, even though auto-merge is on.
    step(&scheduler).await;
    let watched = executor.watched.lock().unwrap().last().unwrap().clone();
    assert!(watched.watch_only);
    assert!(watched.auto_merge);
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::User(Ask::ReviewStopped { .. })
    ));

    // The user merges on GitHub. The next watch reports it, and the factory
    // moves the job on to the next pull request.
    green(&executor, Event::Merged);
    step(&scheduler).await;
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::Implementing { pr: 1 }
    );
}

#[tokio::test]
async fn a_stopped_review_fails_when_its_pull_request_is_closed() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewFailed {
            reason: "the reviewer run ended without structured output".into(),
        },
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", true, true).unwrap();
    let id = to_watching(&store, &executor, &scheduler).await;

    green(&executor, start_review());
    step(&scheduler).await;
    executor.release(1);
    step(&scheduler).await; // reviewer fails -> review stopped

    // The user closes the pull request without merging. The watch reports it
    // and the job fails closed.
    executor.watches.lock().unwrap().push_back(Ok(WatchReport {
        event: Some(Event::ClosedUnmerged),
        seen_until: None,
        merged_by_factory: false,
    }));
    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.stage(),
        Stage::Failed { reason, .. }
            if reason == "the pull request was closed without merging"
    ));
}

#[test]
fn watch_only_reports_only_a_merge_or_close() {
    // A merge or close by the user is the only thing a stopped-review watch
    // acts on.
    assert_eq!(watch_only_event(&Observation::Merged), Some(Event::Merged));
    assert_eq!(
        watch_only_event(&Observation::Closed),
        Some(Event::ClosedUnmerged)
    );
    // Everything else leaves the job in review-stopped. In particular a ready
    // pull request reports nothing, whatever auto-merge says, because the
    // watch never reaches `ready_action`.
    assert_eq!(watch_only_event(&Observation::Ready), None);
    assert_eq!(watch_only_event(&Observation::Conflicted), None);
    assert_eq!(watch_only_event(&Observation::CiFailed(Vec::new())), None);
    assert_eq!(
        watch_only_event(&Observation::Comments {
            text: "please fix".into(),
            seen_until: "2026-11-20T00:00:00Z".into(),
        }),
        None
    );
    assert_eq!(watch_only_event(&Observation::Nothing), None);
}

#[tokio::test]
async fn the_review_cap_sends_the_job_to_the_user() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewChanges {
            feedback: "one more thing".into(),
        },
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", true, true).unwrap();
    let id = to_watching(&store, &executor, &scheduler).await;

    for round in 1..=MAX_REVIEW_ROUNDS {
        green(&executor, start_review());
        step(&scheduler).await;
        executor.release(1);
        step(&scheduler).await; // review asks for changes
        if round == MAX_REVIEW_ROUNDS {
            break;
        }
        // Address the review, then go green again for the next pass.
        executor.release(1);
        step(&scheduler).await; // fix commits -> verifying
        step(&scheduler).await; // checks -> pr-open
        step(&scheduler).await; // publish -> ci-and-review
    }

    let job = store.get(id).unwrap().job;
    assert_eq!(job.reviews().len(), MAX_REVIEW_ROUNDS as usize);
    assert!(matches!(
        job.turn(),
        Turn::User(Ask::ReviewStopped { reason, .. }) if reason.contains("review rounds")
    ));
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
}

mod purge {
    use super::*;
    use factory_server::FINISHED_JOB_DAYS;

    struct Fixture {
        store: Arc<Store>,
        scheduler: Arc<Scheduler<FakeExecutor>>,
        data: tempfile::TempDir,
        db: rusqlite::Connection,
    }

    fn fixture() -> Fixture {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("factory.db");
        let store = Arc::new(Store::open(&path).unwrap());
        let scheduler = Scheduler::new(
            Arc::clone(&store),
            Arc::new([].into_iter().collect()),
            FakeExecutor::new(|_| questions()),
            Arc::new(Notify::new()),
            data.path().to_owned(),
            mcp_runtime(&store).0,
        );
        let db = rusqlite::Connection::open(&path).unwrap();
        Fixture {
            store,
            scheduler,
            data,
            db,
        }
    }

    impl Fixture {
        /// Creates a job with a log line, a session, an image row and file, a
        /// `job_files` row and file, and a work directory, created `days` days
        /// ago.
        fn job(&self, days: u32) -> i64 {
            let id = self.store.create("maestro", "alice", "x").unwrap();
            self.db
                .execute(
                    "UPDATE jobs SET created_at = datetime('now', ?1) WHERE id = ?2",
                    rusqlite::params![format!("-{days} days"), id],
                )
                .unwrap();
            self.store.append_factory_log(id, "hello").unwrap();
            self.store
                .session_uuid(
                    id,
                    factory_core::Session::Planning,
                    factory_core::DEFAULT_AGENT,
                )
                .unwrap();
            self.store.add_images(id, &["image-1.png".into()]).unwrap();
            let images = self.dir(id).join("images");
            std::fs::create_dir_all(&images).unwrap();
            std::fs::write(images.join("image-1.png"), b"png").unwrap();
            self.store
                .add_files(
                    id,
                    &[factory_server::JobFile {
                        name: "file-1.txt".into(),
                        original_name: "server.log".into(),
                    }],
                )
                .unwrap();
            let files = self.dir(id).join("files");
            std::fs::create_dir_all(&files).unwrap();
            std::fs::write(files.join("file-1.txt"), b"log").unwrap();
            std::fs::create_dir_all(self.dir(id).join("work")).unwrap();
            std::fs::create_dir_all(self.dir(id).join("claude")).unwrap();
            std::fs::create_dir_all(self.dir(id).join("codex")).unwrap();
            id
        }

        fn dir(&self, id: i64) -> std::path::PathBuf {
            self.data.path().join("jobs").join(id.to_string())
        }

        fn exists(&self, id: i64) -> bool {
            self.store.get(id).is_ok()
        }

        fn rows(&self, table: &str, id: i64) -> i64 {
            self.db
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE job_id = ?1"),
                    [id],
                    |row| row.get(0),
                )
                .unwrap()
        }

        fn finish(&self, id: i64, how: &str) {
            let store = &self.store;
            match how {
                "cancelled" => {
                    store.apply(id, Event::Cancel).unwrap();
                }
                "failed" => {
                    store.apply(id, Event::Start).unwrap();
                    store
                        .apply(id, Event::AgentFailed { reason: "x".into() })
                        .unwrap();
                }
                _ => {
                    for event in [
                        Event::Start,
                        Event::Agent(AgentResult::Clarified),
                        Event::Agent(AgentResult::Spec("s".into())),
                        Event::Approve,
                        Event::Agent(AgentResult::Plan(vec![factory_core::PlannedPr {
                            title: "t".into(),
                            summary: "s".into(),
                            number: None,
                            description: None,
                            notes: None,
                        }])),
                        Event::Approve,
                        Event::Agent(AgentResult::CommitsReady(ready())),
                        Event::ChecksPassed,
                        Event::Published { number: 3 },
                        Event::Merged,
                    ] {
                        store.apply(id, event).unwrap();
                    }
                }
            }
            assert!(
                store.get(id).unwrap().job.is_finished(),
                "{how} is finished"
            );
        }
    }

    #[test]
    fn the_age_limit_is_30_days() {
        assert_eq!(FINISHED_JOB_DAYS, 30);
    }

    #[tokio::test]
    async fn finished_jobs_are_deleted_after_30_days_and_not_before() {
        let f = fixture();
        let young = f.job(29);
        let old = f.job(31);
        for id in [young, old] {
            f.finish(id, "cancelled");
        }
        f.scheduler.tick().await.unwrap();
        assert!(f.exists(young));
        assert!(!f.exists(old));
    }

    #[tokio::test]
    async fn merged_failed_and_cancelled_jobs_are_all_deleted() {
        let f = fixture();
        let ids: Vec<i64> = ["merged", "failed", "cancelled"]
            .into_iter()
            .map(|how| {
                let id = f.job(31);
                f.finish(id, how);
                id
            })
            .collect();
        f.scheduler.tick().await.unwrap();
        for id in ids {
            assert!(!f.exists(id));
        }
    }

    #[tokio::test]
    async fn unfinished_jobs_are_kept_however_old() {
        let f = fixture();
        let requested = f.job(400);
        let waiting = f.job(400);
        f.store.apply(waiting, Event::Start).unwrap();
        f.store
            .apply(waiting, Event::Agent(AgentResult::Clarified))
            .unwrap();
        f.scheduler.tick().await.unwrap();
        assert!(f.exists(requested));
        assert!(f.exists(waiting));
        assert!(f.dir(waiting).exists());
    }

    #[tokio::test]
    async fn deleting_a_job_removes_its_rows_images_and_directory() {
        let f = fixture();
        let old = f.job(31);
        let other = f.job(1);
        f.finish(old, "cancelled");
        f.scheduler.tick().await.unwrap();

        for table in ["logs", "sessions", "job_images", "job_files"] {
            assert_eq!(f.rows(table, old), 0, "{table}");
            assert_eq!(f.rows(table, other), 1, "{table}");
        }
        assert_eq!(f.rows("job_events", old), 0);
        assert!(f.rows("job_events", other) > 0);
        assert!(!f.dir(old).exists());
        assert!(f.dir(other).join("images/image-1.png").exists());
        assert!(f.dir(other).join("files/file-1.txt").exists());
        assert_eq!(f.store.images(old).unwrap(), Vec::<String>::new());
        assert_eq!(
            f.store.files(old).unwrap(),
            [] as [factory_server::JobFile; 0]
        );
    }

    #[tokio::test]
    async fn a_job_with_no_directory_is_still_deleted() {
        let f = fixture();
        let id = f.store.create("maestro", "alice", "x").unwrap();
        f.db.execute(
            "UPDATE jobs SET created_at = datetime('now', '-40 days')",
            [],
        )
        .unwrap();
        f.finish(id, "cancelled");
        f.scheduler.tick().await.unwrap();
        assert!(!f.exists(id));
    }

    #[tokio::test]
    async fn project_totals_survive_the_deletion_of_a_job() {
        let f = fixture();
        let id = f.job(31);
        f.store.add_cost(id, 0.5, false).unwrap();
        f.finish(id, "merged");
        f.scheduler.tick().await.unwrap();
        assert!(!f.exists(id));

        let totals = f.store.project_totals("maestro").unwrap();
        assert_eq!((totals.jobs_created, totals.jobs_merged), (1, 1));
        assert!((totals.usd - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn one_failing_job_does_not_stop_the_others() {
        let f = fixture();
        let first = f.job(31);
        let stuck = f.job(31);
        let last = f.job(31);
        for id in [first, stuck, last] {
            f.finish(id, "cancelled");
        }
        // remove_dir_all fails on a file, so this job cannot be deleted.
        std::fs::remove_dir_all(f.dir(stuck)).unwrap();
        std::fs::write(f.dir(stuck), b"not a directory").unwrap();

        f.scheduler.tick().await.unwrap();
        assert!(!f.exists(first));
        assert!(f.exists(stuck));
        assert!(!f.exists(last));
        assert_eq!(f.rows("logs", stuck), 1);
    }

    impl Fixture {
        fn has_work(&self, id: i64) -> bool {
            self.dir(id).join("work").exists()
        }

        fn has_claude(&self, id: i64) -> bool {
            self.dir(id).join("claude").exists()
        }

        fn has_codex(&self, id: i64) -> bool {
            self.dir(id).join("codex").exists()
        }

        /// The number of cleanup lines the merged-job cleanup wrote to the job
        /// log.
        fn cleanup_lines(&self, id: i64) -> usize {
            self.store
                .logs(id, 1000)
                .unwrap()
                .into_iter()
                .filter(|line| line.text.contains("free disk space"))
                .count()
        }
    }

    #[tokio::test]
    async fn a_merged_job_loses_its_checkout_and_session() {
        let f = fixture();
        let id = f.job(1);
        f.store.add_cost(id, 0.25, false).unwrap();
        f.finish(id, "merged");
        f.scheduler.tick().await.unwrap();

        assert!(!f.has_work(id));
        assert!(!f.has_claude(id));
        assert!(!f.has_codex(id));
        assert!(f.dir(id).join("images/image-1.png").exists());
        assert!(f.dir(id).join("files/file-1.txt").exists());
        assert!(f.exists(id));
        assert_eq!(f.rows("job_images", id), 1);
        assert_eq!(f.rows("job_files", id), 1);
        assert!(f.rows("logs", id) >= 1);
        assert!(f.rows("job_events", id) > 0);
        assert!((f.store.cost_usd(id).unwrap() - 0.25).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_merged_job_from_before_the_change_is_cleaned_in_one_tick() {
        let f = fixture();
        let id = f.job(1);
        f.finish(id, "merged");
        // Both directories exist before the first pass, as for a job merged
        // before this cleanup shipped.
        assert!(f.has_work(id));
        assert!(f.has_claude(id));

        f.scheduler.tick().await.unwrap();

        assert!(!f.has_work(id));
        assert!(!f.has_claude(id));
    }

    #[tokio::test]
    async fn failed_cancelled_and_unfinished_jobs_keep_their_directories() {
        let f = fixture();
        let failed = f.job(1);
        let cancelled = f.job(1);
        let unfinished = f.job(1);
        f.finish(failed, "failed");
        f.finish(cancelled, "cancelled");

        f.scheduler.tick().await.unwrap();

        for id in [failed, cancelled, unfinished] {
            assert!(f.has_work(id), "job {id} keeps work");
            assert!(f.has_claude(id), "job {id} keeps claude");
        }
    }

    #[tokio::test]
    async fn one_failing_cleanup_does_not_stop_the_others_and_a_later_pass_succeeds() {
        let f = fixture();
        let first = f.job(1);
        let stuck = f.job(1);
        let last = f.job(1);
        for id in [first, stuck, last] {
            f.finish(id, "merged");
        }
        // A file where `work/` should be a directory makes remove_dir_all fail.
        std::fs::remove_dir_all(f.dir(stuck).join("work")).unwrap();
        std::fs::write(f.dir(stuck).join("work"), b"not a directory").unwrap();

        f.scheduler.tick().await.unwrap();

        assert!(!f.has_work(first) && !f.has_claude(first));
        assert!(!f.has_work(last) && !f.has_claude(last));
        // The stuck job's work could not be removed, so the job is not done and
        // wrote no cleanup line.
        assert_eq!(f.cleanup_lines(stuck), 0);

        // Remove the cause and the next pass finishes the cleanup.
        std::fs::remove_file(f.dir(stuck).join("work")).unwrap();
        f.scheduler.tick().await.unwrap();
        assert!(!f.has_work(stuck));
        assert!(!f.has_claude(stuck));
        assert_eq!(f.cleanup_lines(stuck), 1);
    }

    #[tokio::test]
    async fn the_cleanup_line_is_written_exactly_once() {
        let f = fixture();
        let id = f.job(1);
        f.finish(id, "merged");

        for _ in 0..4 {
            f.scheduler.tick().await.unwrap();
        }

        assert_eq!(f.cleanup_lines(id), 1);
    }

    #[tokio::test]
    async fn a_merged_job_over_30_days_is_deleted_whether_or_not_it_was_cleaned() {
        let f = fixture();
        let cleaned = f.job(31);
        let intact = f.job(31);
        f.finish(cleaned, "merged");
        f.finish(intact, "merged");
        // Simulate a job already cleaned by an earlier pass before it expired.
        std::fs::remove_dir_all(f.dir(cleaned).join("work")).unwrap();
        std::fs::remove_dir_all(f.dir(cleaned).join("claude")).unwrap();

        f.scheduler.tick().await.unwrap();

        assert!(!f.exists(cleaned));
        assert!(!f.exists(intact));
        assert!(!f.dir(cleaned).exists());
        assert!(!f.dir(intact).exists());
    }
}

fn models(clarify: &str, spec: &str, plan: &str, implement: &str) -> factory_server::StageModels {
    let some = |name: &str| Some(name.to_owned());
    factory_server::StageModels {
        clarify: some(clarify),
        spec: some(spec),
        plan: some(plan),
        implement: some(implement),
        review: None,
    }
}

/// Asks once, then clarifies, and answers every other stage in one run.
fn one_pass(request: &AgentRequest) -> Event {
    match request.expect {
        Expect::Clarify if request.prompt.contains("answered") => {
            Event::Agent(AgentResult::Clarified)
        }
        Expect::Clarify => questions(),
        Expect::Spec => Event::Agent(AgentResult::Spec("spec".into())),
        Expect::Plan => Event::Agent(AgentResult::Plan(vec![factory_core::PlannedPr {
            title: "a: b".into(),
            summary: "c".into(),
            number: None,
            description: None,
            notes: None,
        }])),
        Expect::Commits | Expect::FixCi => Event::Agent(AgentResult::CommitsReady(ready())),
        Expect::Review => unreachable!("this job keeps llm review off, so no reviewer runs"),
        Expect::FailureAnalysis | Expect::DiffCheck => unreachable!("this job is no Renovate job"),
    }
}

fn model_of(request: &AgentRequest) -> &str {
    request.model.as_deref().unwrap_or("")
}

#[tokio::test]
async fn a_project_without_models_passes_none() {
    let (store, executor, scheduler) = setup(one_pass);
    store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(1);

    step(&scheduler).await;
    assert_eq!(executor.requests()[0].model, None);
}

#[tokio::test]
async fn an_empty_stage_model_passes_none() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_models("maestro", &models("", "s", "p", "i"))
        .unwrap();
    store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(1);

    step(&scheduler).await;
    assert_eq!(executor.requests()[0].model.as_deref(), Some(""));
}

#[tokio::test]
async fn each_stage_runs_with_its_own_model() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_models(
            "maestro",
            &models("c-model", "s-model", "p-model", "i-model"),
        )
        .unwrap();
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(10);

    step(&scheduler).await;
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    step(&scheduler).await;
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    let used: Vec<&str> = requests.iter().map(model_of).collect();
    assert_eq!(
        used,
        ["c-model", "c-model", "s-model", "p-model", "i-model"]
    );
}

#[tokio::test]
async fn a_project_does_not_use_the_models_of_another() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let executor = FakeExecutor::new(|_| questions());
    let project = |repo: &str| Project {
        repo: repo.into(),
        base: "main".into(),
        checks: None,
        models: factory_server::StageModels::default(),
        auto_merge: false,
        llm_review: false,
    };
    let projects: Projects = [
        ("maestro".to_owned(), project("Badger-Embedded/maestro")),
        ("factory".to_owned(), project("moonthebot/factory")),
    ]
    .into_iter()
    .collect();
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        Arc::new(projects),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
        mcp_runtime(&store).0,
    );
    store
        .save_project_models("maestro", &models("maestro-c", "", "", ""))
        .unwrap();
    store
        .save_project_models("factory", &models("factory-c", "", "", ""))
        .unwrap();
    store.create("maestro", "alice", "One").unwrap();
    store.create("factory", "alice", "Two").unwrap();
    executor.release(2);

    step(&scheduler).await;
    let mut used: Vec<(String, String)> = executor
        .requests()
        .iter()
        .map(|r| (r.repo.clone(), model_of(r).to_owned()))
        .collect();
    used.sort();
    assert_eq!(
        used,
        [
            ("Badger-Embedded/maestro".to_owned(), "maestro-c".to_owned()),
            ("moonthebot/factory".to_owned(), "factory-c".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_model_change_at_a_gate_reaches_the_next_run() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_models("maestro", &models("c1", "s1", "p1", "i1"))
        .unwrap();
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(10);

    step(&scheduler).await;
    store
        .save_project_models("maestro", &models("c2", "s2", "p2", "i2"))
        .unwrap();
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    step(&scheduler).await;
    step(&scheduler).await;
    store
        .save_project_models("maestro", &models("c3", "s3", "p3", "i3"))
        .unwrap();
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    store
        .save_project_models("maestro", &models("c4", "s4", "p4", "i4"))
        .unwrap();
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    let used: Vec<&str> = requests.iter().map(model_of).collect();
    assert_eq!(used, ["c1", "c2", "s2", "p3", "i4"]);
}

#[tokio::test]
async fn spec_and_plan_revisions_use_their_stage_model() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_models("maestro", &models("c", "s", "p", "i"))
        .unwrap();
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    store.apply(id, Event::Start).unwrap();
    store
        .apply(id, Event::Agent(AgentResult::Clarified))
        .unwrap();
    executor.release(10);

    step(&scheduler).await;
    store
        .apply(
            id,
            Event::RequestChanges {
                feedback: "Shorter.".into(),
            },
        )
        .unwrap();
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    store
        .apply(
            id,
            Event::RequestChanges {
                feedback: "Split it.".into(),
            },
        )
        .unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    let used: Vec<&str> = requests.iter().map(model_of).collect();
    assert_eq!(used, ["s", "s", "p", "p"]);
    assert!(requests[1].prompt.contains("Shorter."));
    assert!(requests[3].prompt.contains("Split it."));
}

#[tokio::test]
async fn check_fixes_ci_fixes_and_review_runs_use_the_implement_model() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    store
        .save_project_models("maestro", &models("c", "s", "p", "i"))
        .unwrap();
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    executor
        .checks
        .lock()
        .unwrap()
        .push_back(Ok(Event::ChecksFailed {
            report: "clippy".into(),
        }));

    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::Agent(factory_core::Task::FixChecks { .. })
    ));
    executor.release(1);
    step(&scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    for event in [
        Event::CiFailed { log: "red".into() },
        Event::ReviewComments {
            comments: "Needs a test.".into(),
        },
    ] {
        executor.watches.lock().unwrap().push_back(Ok(WatchReport {
            event: Some(event),
            seen_until: None,
            merged_by_factory: false,
        }));
        step(&scheduler).await;
        executor.release(1);
        step(&scheduler).await;
        step(&scheduler).await;
        step(&scheduler).await;
    }

    let used: Vec<String> = executor
        .requests()
        .iter()
        .map(|r| model_of(r).to_owned())
        .collect();
    assert_eq!(used.len(), 4, "{used:?}");
    assert!(used.iter().all(|model| model == "i"), "{used:?}");
}

fn effort_of(request: &AgentRequest) -> &str {
    request
        .effort
        .as_deref()
        .expect("the scheduler always sets an effort")
}

fn stage_efforts(
    clarify: &str,
    spec: &str,
    plan: &str,
    implement: &str,
    review: &str,
) -> StageEfforts {
    StageEfforts {
        clarify: clarify.into(),
        spec: spec.into(),
        plan: plan.into(),
        implement: implement.into(),
        review: review.into(),
    }
}

#[tokio::test]
async fn a_project_without_effort_runs_every_stage_at_medium() {
    let (store, executor, scheduler) = setup(one_pass);
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(10);

    step(&scheduler).await;
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    step(&scheduler).await;
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;

    // Clarify, the clarify answer, spec, plan and implement all run at medium.
    let requests = executor.requests();
    let used: Vec<&str> = requests.iter().map(effort_of).collect();
    assert_eq!(used, [Agent::Claude.default_effort(); 5]);
}

#[tokio::test]
async fn each_stage_runs_with_its_saved_effort() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("low", "high", "xhigh", "max", "medium"),
            &StageAgents::default(),
        )
        .unwrap();
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(10);

    step(&scheduler).await;
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    step(&scheduler).await;
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    let used: Vec<&str> = requests.iter().map(effort_of).collect();
    assert_eq!(used, ["low", "low", "high", "xhigh", "max"]);
}

#[tokio::test]
async fn an_effort_change_reaches_the_next_run() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("low", "low", "low", "low", "low"),
            &StageAgents::default(),
        )
        .unwrap();
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(10);

    step(&scheduler).await;
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("high", "high", "high", "high", "high"),
            &StageAgents::default(),
        )
        .unwrap();
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    step(&scheduler).await;
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;

    // The first clarify ran before the change; every run after it uses high.
    let requests = executor.requests();
    let used: Vec<&str> = requests.iter().map(effort_of).collect();
    assert_eq!(used, ["low", "high", "high", "high", "high"]);
}

#[tokio::test]
async fn check_fixes_ci_fixes_and_review_runs_use_the_implement_effort() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("low", "low", "low", "high", "low"),
            &StageAgents::default(),
        )
        .unwrap();
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    executor
        .checks
        .lock()
        .unwrap()
        .push_back(Ok(Event::ChecksFailed {
            report: "clippy".into(),
        }));

    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::Agent(factory_core::Task::FixChecks { .. })
    ));
    executor.release(1);
    step(&scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    for event in [
        Event::CiFailed { log: "red".into() },
        Event::ReviewComments {
            comments: "Needs a test.".into(),
        },
    ] {
        executor.watches.lock().unwrap().push_back(Ok(WatchReport {
            event: Some(event),
            seen_until: None,
            merged_by_factory: false,
        }));
        step(&scheduler).await;
        executor.release(1);
        step(&scheduler).await;
        step(&scheduler).await;
        step(&scheduler).await;
    }

    let used: Vec<String> = executor
        .requests()
        .iter()
        .map(|r| effort_of(r).to_owned())
        .collect();
    assert_eq!(used.len(), 4, "{used:?}");
    assert!(used.iter().all(|effort| effort == "high"), "{used:?}");
}

#[tokio::test]
async fn the_review_run_uses_the_review_effort() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewApproved,
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", true, true).unwrap();
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("low", "low", "low", "low", "xhigh"),
            &StageAgents::default(),
        )
        .unwrap();
    to_watching(&store, &executor, &scheduler).await;

    green(&executor, start_review());
    step(&scheduler).await;
    executor.release(1);
    step(&scheduler).await;

    let requests = executor.requests();
    let review = requests.last().unwrap();
    assert_eq!(review.expect, Expect::Review);
    assert_eq!(effort_of(review), "xhigh");
}

#[tokio::test]
async fn the_planning_stages_pass_the_agent_from_the_stage_mapping() {
    // Claude is the one available agent, so clarify, the clarify answer, spec,
    // plan and implement each run on Claude.
    let (store, executor, scheduler) = setup(one_pass);
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(10);

    step(&scheduler).await;
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    step(&scheduler).await;
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;
    store.apply(id, Event::Approve).unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    let stages: Vec<Expect> = requests.iter().map(|r| r.expect).collect();
    assert_eq!(
        stages,
        [
            Expect::Clarify,
            Expect::Clarify,
            Expect::Spec,
            Expect::Plan,
            Expect::Commits
        ]
    );
    assert!(
        requests.iter().all(|r| r.agent == Agent::Claude),
        "{:?}",
        requests.iter().map(|r| r.agent).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn check_fixes_ci_fixes_and_review_runs_pass_the_implement_agent() {
    // The check fix, CI fix and review-comment runs map to the implement stage,
    // like the model and effort. Each passes the implement agent, which is
    // Claude, the one available agent.
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    executor
        .checks
        .lock()
        .unwrap()
        .push_back(Ok(Event::ChecksFailed {
            report: "clippy".into(),
        }));

    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::Agent(factory_core::Task::FixChecks { .. })
    ));
    executor.release(1);
    step(&scheduler).await;
    step(&scheduler).await;
    step(&scheduler).await;
    for event in [
        Event::CiFailed { log: "red".into() },
        Event::ReviewComments {
            comments: "Needs a test.".into(),
        },
    ] {
        executor.watches.lock().unwrap().push_back(Ok(WatchReport {
            event: Some(event),
            seen_until: None,
            merged_by_factory: false,
        }));
        step(&scheduler).await;
        executor.release(1);
        step(&scheduler).await;
        step(&scheduler).await;
        step(&scheduler).await;
    }

    let requests = executor.requests();
    // The implement run, the check fix, the CI fix and the review-comment run:
    // every one runs on Claude.
    assert_eq!(requests.len(), 4, "{} requests", requests.len());
    assert!(
        requests.iter().all(|r| r.agent == Agent::Claude),
        "{:?}",
        requests.iter().map(|r| r.agent).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_resumed_session_keeps_the_agent_recorded_on_it() {
    // The scheduler reads the agent the store recorded when the session began.
    // A session recorded on an agent other than the stage's current one still
    // runs on the recorded agent.
    let (store, executor, scheduler) = setup(one_pass);
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    // Record the planning session on Codex before the scheduler runs it, as a
    // session started under a different agent would be.
    let (_, recorded) = store
        .session_uuid(id, factory_core::Session::Planning, Agent::Codex)
        .unwrap();
    assert_eq!(recorded, Agent::Codex);
    executor.release(1);

    step(&scheduler).await;
    let clarify = &executor.requests()[0];
    assert_eq!(clarify.expect, Expect::Clarify);
    assert_eq!(clarify.agent, Agent::Codex);
}

fn copilot_implement() -> StageAgents {
    StageAgents {
        implement: "copilot".into(),
        ..StageAgents::default()
    }
}

fn implement_model(model: &str) -> StageModels {
    StageModels {
        implement: Some(model.to_owned()),
        ..StageModels::default()
    }
}

fn implement_effort(level: &str) -> StageEfforts {
    StageEfforts {
        implement: level.to_owned(),
        ..StageEfforts::default()
    }
}

fn to_implement(store: &Store) -> i64 {
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    store.apply(id, Event::Start).unwrap();
    store
        .apply(id, Event::Agent(AgentResult::Clarified))
        .unwrap();
    store
        .apply(id, Event::Agent(AgentResult::Spec("spec".into())))
        .unwrap();
    store.apply(id, Event::Approve).unwrap();
    let planned = factory_core::PlannedPr {
        title: "maestrod: add a reboot endpoint".into(),
        summary: "POST /reboot".into(),
        number: None,
        description: None,
        notes: None,
    };
    store
        .apply(id, Event::Agent(AgentResult::Plan(vec![planned])))
        .unwrap();
    store.apply(id, Event::Approve).unwrap();
    id
}

#[tokio::test]
async fn a_resumed_implement_session_keeps_its_model_and_effort_when_the_stage_moves_to_copilot() {
    let (store, executor, scheduler) = setup(|request| {
        if request.prompt.contains("answered") {
            Event::Agent(AgentResult::CommitsReady(ready()))
        } else {
            questions()
        }
    });
    store
        .save_project_models("maestro", &implement_model("sonnet"))
        .unwrap();
    store
        .save_project_efforts(
            "maestro",
            &implement_effort("high"),
            &StageAgents::default(),
        )
        .unwrap();
    let id = to_implement(&store);
    executor.release(2);
    step(&scheduler).await;

    store
        .save_project_agents("maestro", &copilot_implement())
        .unwrap();
    store
        .save_project_models("maestro", &implement_model("claude-sonnet-5.5"))
        .unwrap();
    store
        .save_project_efforts("maestro", &implement_effort("low"), &copilot_implement())
        .unwrap();
    store.apply(id, Event::Answer(vec!["rpi5".into()])).unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    assert_eq!(requests[0].expect, Expect::Commits);
    assert_eq!(requests[1].expect, Expect::Commits);
    assert_eq!(requests[1].agent, Agent::Claude);
    assert_eq!(model_of(&requests[1]), "sonnet");
    assert_eq!(effort_of(&requests[1]), "high");
    let notes: Vec<String> = store
        .logs(id, 50)
        .unwrap()
        .into_iter()
        .map(|entry| entry.text)
        .filter(|text| text.contains("stays on Claude"))
        .collect();
    assert_eq!(
        notes,
        [
            "This session started on Claude, so it stays on Claude with its earlier model. \
          The implement stage is now set to Copilot, which new sessions use."
        ]
    );
}

#[tokio::test]
async fn a_session_with_no_recorded_settings_resumes_with_no_model_and_the_default_effort() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_agents("maestro", &copilot_implement())
        .unwrap();
    store
        .save_project_models("maestro", &implement_model("claude-sonnet-5.5"))
        .unwrap();
    store
        .save_project_efforts("maestro", &implement_effort("high"), &copilot_implement())
        .unwrap();
    let id = to_implement(&store);
    store
        .session_uuid(id, factory_core::Session::Pr(0), Agent::Claude)
        .unwrap();
    executor.release(1);
    step(&scheduler).await;

    let request = &executor.requests()[0];
    assert_eq!(request.agent, Agent::Claude);
    assert_eq!(request.model, None);
    assert_eq!(effort_of(request), Agent::Claude.default_effort());
}

#[tokio::test]
async fn a_fresh_copilot_stage_runs_with_its_model() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_agents("maestro", &copilot_implement())
        .unwrap();
    store
        .save_project_models("maestro", &implement_model("claude-sonnet-5.5"))
        .unwrap();
    to_implement(&store);
    executor.release(1);
    step(&scheduler).await;

    let request = &executor.requests()[0];
    assert_eq!(request.agent, Agent::Copilot);
    assert_eq!(model_of(request), "claude-sonnet-5.5");
}

#[tokio::test]
async fn a_codex_run_stores_its_thread_id_and_the_next_run_resumes_on_it() {
    // Codex names its own session, so the scheduler learns the thread id from the
    // run, stores it against the session, and passes it to the next run of that
    // session as `agent_session`. The runner resumes on it.
    let (store, executor, scheduler) = setup(one_pass);
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    store
        .session_uuid(id, factory_core::Session::Planning, Agent::Codex)
        .unwrap();
    // The first clarify run streams its thread id back, the way Codex reports it
    // on `thread.started`.
    executor.report_session("01998d1a-thread-id");
    executor.release(2);

    // The clarify run asks questions and stores its thread id.
    step(&scheduler).await;
    assert_eq!(
        store
            .agent_session(id, factory_core::Session::Planning)
            .unwrap()
            .as_deref(),
        Some("01998d1a-thread-id")
    );

    // The answer run resumes the same session, so it carries the stored thread id.
    store
        .apply(id, Event::Answer(vec!["a button".into()]))
        .unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    assert_eq!(requests[0].expect, Expect::Clarify);
    // Nothing was stored before the first run, so it had no thread id to resume.
    assert_eq!(requests[0].agent_session, None);
    assert_eq!(requests[1].expect, Expect::Clarify);
    assert_eq!(
        requests[1].agent_session.as_deref(),
        Some("01998d1a-thread-id")
    );
}

#[tokio::test]
async fn a_codex_run_that_reports_no_thread_id_leaves_the_next_run_fresh() {
    // A run killed before `thread.started` stores no id, so the next run of the
    // session starts fresh rather than resuming a session that never began.
    let (store, executor, scheduler) = setup(one_pass);
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    store
        .session_uuid(id, factory_core::Session::Planning, Agent::Codex)
        .unwrap();
    // No `report_session`: the first run ends without reporting a thread id.
    executor.release(2);

    step(&scheduler).await;
    assert_eq!(
        store
            .agent_session(id, factory_core::Session::Planning)
            .unwrap(),
        None
    );

    store
        .apply(id, Event::Answer(vec!["a button".into()]))
        .unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    assert_eq!(requests[1].agent_session, None);
}

#[tokio::test]
async fn a_job_from_before_fresh_sessions_resumes_its_planning_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&path).unwrap());
    let executor = FakeExecutor::new(|request| match request.expect {
        Expect::Clarify => Event::Agent(AgentResult::Clarified),
        Expect::Spec => Event::Agent(AgentResult::Spec("the spec".into())),
        _ => Event::Agent(AgentResult::Plan(vec![factory_core::PlannedPr {
            title: "a: b".into(),
            summary: "c".into(),
            number: None,
            description: None,
            notes: None,
        }])),
    });
    let projects: Projects = [(
        "maestro".to_owned(),
        Project {
            repo: "Badger-Embedded/maestro".into(),
            base: "main".into(),
            checks: None,
            models: factory_server::StageModels::default(),
            auto_merge: false,
            llm_review: false,
        },
    )]
    .into_iter()
    .collect();
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        Arc::new(projects),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
        mcp_runtime(&store).0,
    );
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE jobs SET job = json_remove(job, '$.fresh_sessions', '$.answers') WHERE id = ?1",
        [id],
    )
    .unwrap();
    executor.release(10);

    for _ in 0..2 {
        scheduler.tick().await.unwrap();
        wait_idle(&scheduler).await;
    }
    store.apply(id, Event::Approve).unwrap();
    scheduler.tick().await.unwrap();
    wait_idle(&scheduler).await;

    let requests = executor.requests();
    let expects: Vec<Expect> = requests.iter().map(|r| r.expect).collect();
    assert_eq!(expects, [Expect::Clarify, Expect::Spec, Expect::Plan]);
    assert!(
        requests
            .iter()
            .all(|r| r.session_id == requests[0].session_id)
    );
    assert!(!requests[2].prompt.contains("The approved spec:"));
}

/// Takes a job to the rerun turn of a pull request with a failed CI.
async fn to_rerun(
    store: &Store,
    executor: &FakeExecutor,
    scheduler: &Arc<Scheduler<FakeExecutor>>,
) -> i64 {
    let id = to_commits_ready(store, executor, scheduler).await;
    step(scheduler).await;
    step(scheduler).await;
    executor.watches.lock().unwrap().push_back(Ok(WatchReport {
        event: Some(Event::CiFailed {
            log: "flaky test".into(),
        }),
        seen_until: None,
        merged_by_factory: false,
    }));
    step(scheduler).await;
    executor.release(1);
    step(scheduler).await;
    id
}

fn rerun_responder(request: &AgentRequest) -> Event {
    match request.expect {
        Expect::FixCi => Event::Agent(AgentResult::RerunCi {
            reason: "the failure was a network timeout".into(),
        }),
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    }
}

#[tokio::test]
async fn a_rerun_request_reaches_the_executor_and_its_reason_is_logged() {
    let (store, executor, scheduler) = setup(rerun_responder);
    quick_polls(&store);
    let id = to_rerun(&store, &executor, &scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::RerunCi { .. }
    ));

    step(&scheduler).await;
    let requests = executor.rerun_requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        vec![RerunRequest {
            job_id: id,
            repo: "Badger-Embedded/maestro".into(),
            number: 12,
        }]
    );
    let texts: Vec<String> = store
        .logs(id, 50)
        .unwrap()
        .into_iter()
        .map(|entry| entry.text)
        .collect();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("the failure was a network timeout")),
        "{texts:?}"
    );
    assert_eq!(store.get(id).unwrap().job.turn(), &Turn::Factory);
}

#[tokio::test]
async fn a_failed_rerun_is_tried_again() {
    let (store, executor, scheduler) = setup(rerun_responder);
    quick_polls(&store);
    let id = to_rerun(&store, &executor, &scheduler).await;
    executor
        .reruns
        .lock()
        .unwrap()
        .push_back(Err("gh: rate limited".into()));

    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.turn(),
        Turn::RerunCi { .. }
    ));
    step(&scheduler).await;
    assert_eq!(executor.rerun_requests.lock().unwrap().len(), 2);
    assert_eq!(store.get(id).unwrap().job.turn(), &Turn::Factory);
}

#[tokio::test]
async fn the_watch_after_a_rerun_waits_for_the_poll_interval() {
    let (store, executor, scheduler) = setup(rerun_responder);
    quick_polls(&store);
    to_rerun(&store, &executor, &scheduler).await;
    let watches = executor.watched.lock().unwrap().len();
    store
        .save_settings(&Settings {
            poll_seconds: 3600,
            ..Settings::default()
        })
        .unwrap();

    step(&scheduler).await;
    step(&scheduler).await;
    assert_eq!(executor.rerun_requests.lock().unwrap().len(), 1);
    assert_eq!(executor.watched.lock().unwrap().len(), watches);
}

#[tokio::test]
async fn retrying_a_failed_job_watches_the_pull_request_again() {
    let (store, executor, scheduler) = setup(|_| Event::Agent(AgentResult::CommitsReady(ready())));
    quick_polls(&store);
    let id = to_commits_ready(&store, &executor, &scheduler).await;
    store.apply(id, Event::ChecksPassed).unwrap();
    store.apply(id, Event::Published { number: 50 }).unwrap();
    for _ in 1..MAX_FAILED_ROUNDS {
        store
            .apply(id, Event::CiFailed { log: "fail".into() })
            .unwrap();
        store
            .apply(id, Event::Agent(AgentResult::CommitsReady(ready())))
            .unwrap();
        store.apply(id, Event::ChecksPassed).unwrap();
        store.apply(id, Event::Published { number: 50 }).unwrap();
    }
    store
        .apply(id, Event::CiFailed { log: "last".into() })
        .unwrap();
    assert!(matches!(
        store.get(id).unwrap().job.stage(),
        Stage::Failed { .. }
    ));

    store.apply(id, Event::Retry).unwrap();
    assert_eq!(
        store.get(id).unwrap().job.stage(),
        &Stage::CiAndReview { pr: 0 }
    );

    step(&scheduler).await;
    assert_eq!(executor.watched.lock().unwrap().last().unwrap().number, 50);
}

fn enabled(stages: McpStages) -> McpProjectSettings {
    McpProjectSettings {
        enabled: true,
        stages,
    }
}

fn mcp_stage(request: &AgentRequest) -> Vec<String> {
    request
        .mcp
        .as_ref()
        .map(|mcp| mcp.servers.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_run_with_no_active_server_carries_no_mcp_config() {
    let (store, executor, scheduler) = setup(|_| questions());
    // A server is registered, but the project leaves it off, the default.
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();

    scheduler.tick().await.unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    assert!(
        executor.requests()[0].mcp.is_none(),
        "an off server adds no MCP config"
    );
    assert!(
        !executor.requests()[0].prompt.contains("MCP servers"),
        "a run with no active server gets no MCP paragraph"
    );
}

#[tokio::test]
async fn mcp_servers_reach_only_the_runs_of_their_checked_stages() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::Review => Event::ReviewApproved,
        _ => Event::Agent(AgentResult::CommitsReady(ready())),
    });
    quick_polls(&store);
    store.save_project_settings("maestro", true, true).unwrap();
    store.save_project_models("maestro", &reviewing()).unwrap();

    // Three keyless servers: one for implement only, one for review only, and
    // one turned off that nonetheless checks both stages.
    for name in ["impl-only", "review-only", "off"] {
        store
            .create_mcp_server(name, &format!("https://{name}.example/mcp"), &[])
            .unwrap();
    }
    store
        .save_project_mcp(
            "maestro",
            "impl-only",
            &enabled(McpStages {
                implement: true,
                ..Default::default()
            }),
        )
        .unwrap();
    store
        .save_project_mcp(
            "maestro",
            "review-only",
            &enabled(McpStages {
                review: true,
                ..Default::default()
            }),
        )
        .unwrap();
    store
        .save_project_mcp(
            "maestro",
            "off",
            &McpProjectSettings {
                enabled: false,
                stages: McpStages {
                    implement: true,
                    review: true,
                    ..Default::default()
                },
            },
        )
        .unwrap();

    // The implement run happens inside this helper.
    let id = to_watching(&store, &executor, &scheduler).await;
    let implement = executor
        .requests()
        .into_iter()
        .find(|r| r.expect == Expect::Commits)
        .expect("an implement run");
    assert_eq!(mcp_stage(&implement), ["impl-only"]);
    assert!(
        implement
            .prompt
            .contains("This run has these MCP servers: `impl-only`."),
        "the implement prompt names its active server"
    );
    assert!(!implement.prompt.contains("review-only"));
    assert!(!implement.prompt.contains("`off`"));

    // Drive the review run for the same pull request.
    green(&executor, start_review());
    step(&scheduler).await;
    executor.release(1);
    step(&scheduler).await;
    let review = executor
        .requests()
        .into_iter()
        .rev()
        .find(|r| r.expect == Expect::Review)
        .expect("a review run");
    assert_eq!(mcp_stage(&review), ["review-only"]);
    assert!(
        review
            .prompt
            .contains("This run has these MCP servers: `review-only`."),
        "the review prompt names its active server"
    );
    assert!(!review.prompt.contains("impl-only"));
    assert!(!review.prompt.contains("`off`"));

    // The off server reaches neither run, though it checks both stages.
    assert!(!mcp_stage(&implement).contains(&"off".to_string()));
    assert!(!mcp_stage(&review).contains(&"off".to_string()));
    let _ = id;
}

#[tokio::test]
async fn an_init_line_failure_records_one_warning_and_the_run_goes_on() {
    let (store, executor, scheduler, _tokens) = setup_mcp(|_| questions());
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    store
        .save_project_mcp(
            "maestro",
            "context7",
            &enabled(McpStages {
                clarify: true,
                ..Default::default()
            }),
        )
        .unwrap();
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();
    executor.report_unavailable(vec![McpUnavailable {
        server: "context7".into(),
        reason: "the agent reported it as failed".into(),
    }]);

    scheduler.tick().await.unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    let rows: Vec<_> = store
        .events(id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event == "mcp-unavailable")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].detail.as_deref(),
        Some("context7: the agent reported it as failed")
    );
    // The run did not fail: it reached the clarify questions, so the warning is
    // only a note.
    assert!(matches!(
        store.get(id).unwrap().job.stage(),
        Stage::Clarifying
    ));
}

#[tokio::test]
async fn a_server_whose_secret_cannot_be_decrypted_is_skipped_with_a_warning() {
    // setup_mcp uses an unavailable secret store, so a stored header value never
    // decrypts. The server is then left out of the run and warned about.
    let (store, executor, scheduler, _tokens) = setup_mcp(|_| questions());
    store
        .create_mcp_server(
            "acme",
            "https://mcp.acme.test/mcp",
            &["Authorization".into()],
        )
        .unwrap();
    store
        .save_project_mcp(
            "maestro",
            "acme",
            &enabled(McpStages {
                clarify: true,
                ..Default::default()
            }),
        )
        .unwrap();
    // A stored value exists, but the secret store cannot read it.
    store
        .put_secret(
            SecretRef {
                project: "maestro",
                server: "acme",
                header: "Authorization",
            },
            "local",
            b"ciphertext",
        )
        .unwrap();
    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();

    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;
    assert!(executor.requests()[0].mcp.is_none());
    assert!(!executor.requests()[0].prompt.contains("MCP servers"));
    executor.release(1);
    wait_idle(&scheduler).await;

    let rows: Vec<_> = store
        .events(id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event == "mcp-unavailable")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].detail.as_deref(),
        Some("acme: a stored header value could not be decrypted")
    );
}

#[tokio::test]
async fn a_run_token_is_live_during_the_run_and_revoked_after() {
    let (store, executor, scheduler, tokens) = setup_mcp(|_| questions());
    store
        .create_mcp_server("context7", "https://mcp.context7.com/mcp", &[])
        .unwrap();
    store
        .save_project_mcp(
            "maestro",
            "context7",
            &enabled(McpStages {
                clarify: true,
                ..Default::default()
            }),
        )
        .unwrap();
    store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();

    scheduler.tick().await.unwrap();
    wait_for_requests(&executor, 1).await;
    let mcp = executor.requests()[0]
        .mcp
        .clone()
        .expect("context7 is active for the clarify run");
    assert_eq!(mcp.servers, ["context7"]);
    assert_eq!(mcp.proxy_url, "http://factory:8081");
    assert!(mcp.token.len() >= 64, "a 256-bit token as hex");
    // The worker is still running, so the proxy honours its token.
    assert!(tokens.is_active(&mcp.token));

    executor.release(1);
    wait_idle(&scheduler).await;
    // The worker exited, so the factory revoked the token.
    assert!(!tokens.is_active(&mcp.token));
}

#[tokio::test]
async fn an_edited_queued_job_runs_clarify_with_the_new_text_and_attachments() {
    use std::fs;

    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let executor = FakeExecutor::new(|_| questions());
    let projects: Projects = [(
        "maestro".to_owned(),
        Project {
            repo: "Badger-Embedded/maestro".into(),
            base: "main".into(),
            checks: Some("just lint".into()),
            models: factory_server::StageModels::default(),
            auto_merge: false,
            llm_review: false,
        },
    )]
    .into_iter()
    .collect();
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        Arc::new(projects),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        data.path().to_owned(),
        mcp_runtime(&store).0,
    );

    let id = store
        .create("maestro", "alice", "Add a reboot button")
        .unwrap();

    // The initial attachment set, placed the way the web handler places it.
    let job_dir = data.path().join("jobs").join(id.to_string());
    fs::create_dir_all(job_dir.join("images")).unwrap();
    fs::write(job_dir.join("images").join("image-1.png"), b"one").unwrap();
    fs::write(job_dir.join("images").join("image-2.png"), b"two").unwrap();
    store
        .add_images(id, &["image-1.png".into(), "image-2.png".into()])
        .unwrap();
    fs::create_dir_all(job_dir.join("files")).unwrap();
    fs::write(job_dir.join("files").join("file-1.txt"), b"notes").unwrap();
    store
        .add_files(
            id,
            &[JobFile {
                name: "file-1.txt".into(),
                original_name: "notes.txt".into(),
            }],
        )
        .unwrap();

    // Drop image-1, keep image-2 (renumbered to image-1), add a new image-2,
    // and keep the file. The caller writes this final set to `staging/` first.
    let staging = job_dir.join("staging");
    fs::create_dir_all(staging.join("images")).unwrap();
    fs::write(staging.join("images").join("image-1.png"), b"two").unwrap();
    fs::write(staging.join("images").join("image-2.png"), b"three").unwrap();
    fs::create_dir_all(staging.join("files")).unwrap();
    fs::write(staging.join("files").join("file-1.txt"), b"notes").unwrap();
    store
        .edit_request(
            id,
            "Add a shutdown button".into(),
            data.path(),
            &["image-1.png".into(), "image-2.png".into()],
            &[JobFile {
                name: "file-1.txt".into(),
                original_name: "notes.txt".into(),
            }],
        )
        .unwrap();
    let _ = fs::remove_dir_all(&staging);

    // The post-edit set a run reads: the renumbered kept image, the new one,
    // and the kept file. The removed image's bytes are gone.
    assert_eq!(store.images(id).unwrap(), ["image-1.png", "image-2.png"]);
    assert_eq!(
        fs::read(job_dir.join("images").join("image-1.png")).unwrap(),
        b"two"
    );
    assert_eq!(
        fs::read(job_dir.join("images").join("image-2.png")).unwrap(),
        b"three"
    );
    let mut on_disk: Vec<String> = fs::read_dir(job_dir.join("images"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    assert_eq!(on_disk, ["image-1.png", "image-2.png"]);
    assert_eq!(store.files(id).unwrap().len(), 1);

    scheduler.tick().await.unwrap();
    executor.release(1);
    wait_idle(&scheduler).await;

    let request = &executor.requests()[0];
    assert_eq!(request.job_id, id);
    assert_eq!(request.expect, Expect::Clarify);
    assert!(request.prompt.contains("Add a shutdown button"));
    assert!(!request.prompt.contains("Add a reboot button"));
}

/// A [`Event::StartReview`] on a head no earlier pass read, so the reviewer
/// runs.
fn start_review() -> Event {
    static HEADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = HEADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Event::StartReview {
        head: format!("head{n}"),
    }
}

fn observed(head: &str, ci: factory_core::RenovateCi) -> Event {
    Event::RenovateObserved {
        head: head.into(),
        ci,
        limits: factory_core::RetryLimits { auto: 0, total: 7 },
        auto_merge: false,
    }
}

fn review_stage_settings(store: &Store) {
    store
        .save_project_models(
            "maestro",
            &StageModels {
                clarify: Some("clarify-model".into()),
                review: Some("review-model".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let agents = StageAgents {
        review: "codex".into(),
        ..StageAgents::default()
    };
    store.save_project_agents("maestro", &agents).unwrap();
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("low", "low", "low", "low", "high"),
            &agents,
        )
        .unwrap();
    for name in ["clarify-only", "review-only"] {
        store
            .create_mcp_server(name, &format!("https://{name}.example/mcp"), &[])
            .unwrap();
    }
    for (name, stages) in [
        (
            "clarify-only",
            McpStages {
                clarify: true,
                ..Default::default()
            },
        ),
        (
            "review-only",
            McpStages {
                review: true,
                ..Default::default()
            },
        ),
    ] {
        store
            .save_project_mcp("maestro", name, &enabled(stages))
            .unwrap();
    }
}

#[tokio::test]
async fn renovate_runs_get_their_own_sessions_on_the_review_stage_settings() {
    let (store, executor, scheduler) = setup(|request| match request.expect {
        Expect::FailureAnalysis => Event::Agent(AgentResult::FailureVerdict {
            transient: true,
            reason: "the runner lost its network".into(),
        }),
        _ => Event::AgentFailed {
            reason: "timed out".into(),
        },
    });
    review_stage_settings(&store);
    let id = store
        .create_renovate_job("maestro", 7, "Update serde", "https://example/pull/7")
        .unwrap()
        .unwrap();
    executor.release(10);

    store
        .apply(
            id,
            observed(
                "h1",
                factory_core::RenovateCi::Failed { log: "boom".into() },
            ),
        )
        .unwrap();
    step(&scheduler).await;
    assert_eq!(
        store.get(id).unwrap().job.turn(),
        &Turn::RerunCi {
            reason: "transient failure: the runner lost its network".into()
        }
    );
    store.apply(id, Event::CiRerun).unwrap();
    store
        .apply(
            id,
            observed(
                "h1",
                factory_core::RenovateCi::Passed {
                    details: Some(Box::new(factory_core::PrDiff {
                        diff: "+serde = 1.0.2".into(),
                        body: "Bumps serde".into(),
                    })),
                },
            ),
        )
        .unwrap();
    step(&scheduler).await;
    assert!(matches!(
        store.get(id).unwrap().job.stage(),
        Stage::Failed { .. }
    ));
    store.apply(id, Event::Retry).unwrap();
    step(&scheduler).await;

    let requests = executor.requests();
    let expects: Vec<_> = requests.iter().map(|r| r.expect).collect();
    assert_eq!(
        expects,
        [
            Expect::FailureAnalysis,
            Expect::DiffCheck,
            Expect::DiffCheck
        ]
    );
    assert_ne!(requests[0].session_id, requests[1].session_id);
    assert_eq!(requests[1].session_id, requests[2].session_id);
    for request in &requests {
        assert_eq!(request.agent, Agent::Codex);
        assert_eq!(request.model.as_deref(), Some("review-model"));
        assert_eq!(request.effort.as_deref(), Some("high"));
        assert_eq!(mcp_stage(request), ["review-only"]);
    }
}

fn renovate_pr(number: u64) -> RenovatePr {
    RenovatePr {
        number,
        title: format!("Update dependency {number}"),
        url: format!("https://github.com/Badger-Embedded/maestro/pull/{number}"),
        head: "h1".into(),
    }
}

fn enable_renovate(store: &Store) {
    store
        .save_project_maintenance(
            "maestro",
            &factory_server::Maintenance {
                renovate: true,
                packaging: None,
            },
        )
        .unwrap();
}

/// A scheduler for a project with the Renovate switch on and one pull request
/// from `renovate[bot]`, with no wait between looks.
fn renovate_setup(
    respond: impl Fn(&AgentRequest) -> Event + Send + Sync + 'static,
) -> (
    Arc<Store>,
    Arc<FakeExecutor>,
    Arc<Scheduler<FakeExecutor>>,
    i64,
) {
    let (store, executor, scheduler) = setup(respond);
    quick_polls(&store);
    enable_renovate(&store);
    let id = store
        .create_renovate_job("maestro", 7, "Update serde", "https://example/pull/7")
        .unwrap()
        .unwrap();
    executor.release(50);
    (store, executor, scheduler, id)
}

fn pr_of(store: &Store, id: i64) -> factory_core::RenovatePr {
    store.get(id).unwrap().job.renovate_pr().unwrap().clone()
}

async fn step_until(scheduler: &Arc<Scheduler<FakeExecutor>>, mut done: impl FnMut() -> bool) {
    for _ in 0..80 {
        if done() {
            return;
        }
        step(scheduler).await;
    }
    panic!("the condition never held");
}

fn failing(head: &'static str) -> RenovateNow {
    RenovateNow::Ci(head, RenovateCi::Failed { log: "boom".into() })
}

fn passed(head: &'static str) -> RenovateNow {
    RenovateNow::Ci(
        head,
        RenovateCi::Passed {
            details: Some(Box::new(factory_core::PrDiff {
                diff: "+serde = 1.0.2".into(),
                body: "Bumps serde".into(),
            })),
        },
    )
}

fn needs_you(store: &Store, id: i64) -> Option<RenovateWait> {
    match store.get(id).unwrap().job.turn() {
        Turn::User(Ask::Renovate { state, .. }) => Some(*state),
        _ => None,
    }
}

fn transient_runs(request: &AgentRequest) -> Event {
    match request.expect {
        Expect::FailureAnalysis => Event::Agent(AgentResult::FailureVerdict {
            transient: true,
            reason: "the runner lost its network".into(),
        }),
        _ => Event::AgentFailed {
            reason: "unexpected run".into(),
        },
    }
}

fn verdict(safe: bool) -> impl Fn(&AgentRequest) -> Event + Send + Sync + 'static {
    move |_| {
        Event::Agent(AgentResult::DiffVerdict(Box::new(
            factory_core::DiffVerdict {
                safe,
                reason: if safe {
                    "only a version bump".into()
                } else {
                    "it also edits the build script".into()
                },
                summary: "Bumps serde to 1.0.2".into(),
                from: None,
                to: None,
            },
        )))
    }
}

fn set_limits(store: &Store, auto: u32, total: u32) {
    let mut settings = store.settings().unwrap();
    settings.renovate_auto_reruns = auto;
    settings.renovate_total_reruns = total;
    store.save_settings(&settings).unwrap();
}

#[tokio::test]
async fn discovery_makes_one_job_per_renovate_pull_request() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("factory.db");
    let store = Arc::new(Store::open(&db).unwrap());
    let executor = FakeExecutor::new(|_| questions());
    enable_renovate(&store);
    {
        let mut open = executor.renovate_open.lock().unwrap();
        open.push(("renovate[bot]".into(), "main".into(), renovate_pr(7)));
        open.push(("dependabot[bot]".into(), "main".into(), renovate_pr(8)));
        open.push(("renovate[bot]".into(), "next".into(), renovate_pr(9)));
    }
    let build = |store: &Arc<Store>| {
        let projects: Projects = [(
            "maestro".to_owned(),
            Project {
                repo: "Badger-Embedded/maestro".into(),
                base: "main".into(),
                checks: None,
                models: StageModels::default(),
                auto_merge: false,
                llm_review: false,
            },
        )]
        .into_iter()
        .collect();
        Scheduler::new(
            Arc::clone(store),
            Arc::new(projects),
            Arc::clone(&executor),
            Arc::new(Notify::new()),
            PathBuf::new(),
            mcp_runtime(store).0,
        )
    };

    let scheduler = build(&store);
    step(&scheduler).await;
    let jobs = store.list().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].requested_by, "factory");
    assert_eq!(jobs[0].job.renovate_pr().unwrap().number, 7);

    step(&scheduler).await;
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(
        executor.renovate_listed.lock().unwrap().len(),
        1,
        "a project is listed at most once per discovery interval"
    );

    drop(scheduler);
    let reopened = Arc::new(Store::open(&db).unwrap());
    let scheduler = build(&reopened);
    step(&scheduler).await;
    assert_eq!(executor.renovate_listed.lock().unwrap().len(), 2);
    assert_eq!(reopened.list().unwrap().len(), 1);
}

#[tokio::test]
async fn a_project_without_the_switch_is_never_listed() {
    let (store, executor, scheduler) = setup(|_| questions());
    executor.renovate_open.lock().unwrap().push((
        "renovate[bot]".into(),
        "main".into(),
        renovate_pr(7),
    ));
    step(&scheduler).await;
    assert!(executor.renovate_listed.lock().unwrap().is_empty());
    assert_eq!(store.list().unwrap().len(), 0);
}

#[tokio::test]
async fn failing_ci_is_rerun_three_times_before_an_agent_reads_it() {
    let (store, executor, scheduler, id) = renovate_setup(|request| {
        assert_eq!(request.expect, Expect::FailureAnalysis);
        assert_eq!(request.branch, None);
        Event::Agent(AgentResult::FailureVerdict {
            transient: false,
            reason: "the new API drops a method".into(),
        })
    });
    *executor.renovate_now.lock().unwrap() = failing("h1");
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    step(&scheduler).await;
    step(&scheduler).await;

    assert_eq!(executor.rerun_requests.lock().unwrap().len(), 3);
    assert_eq!(executor.requests().len(), 1);
    assert_eq!(needs_you(&store, id), Some(RenovateWait::Stopped));
    let comments = executor.comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].1.contains("the new API drops a method"));
    assert_eq!(executor.rerun_requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn one_automatic_rerun_sends_the_second_failure_to_the_agent() {
    let (store, executor, scheduler, id) = renovate_setup(transient_runs);
    set_limits(&store, 1, 7);
    *executor.renovate_now.lock().unwrap() = failing("h1");
    step_until(&scheduler, || !executor.requests().is_empty()).await;
    assert_eq!(executor.rerun_requests.lock().unwrap().len(), 1);
    step(&scheduler).await;
    step(&scheduler).await;
    assert_eq!(pr_of(&store, id).reruns, 2, "a transient verdict reruns");
    assert!(executor.comments.lock().unwrap().is_empty());
}

#[tokio::test]
async fn triage_stops_after_the_total_reruns_and_restarts_on_a_new_head_or_retry() {
    let (store, executor, scheduler, id) = renovate_setup(transient_runs);
    *executor.renovate_now.lock().unwrap() = failing("h1");
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    for _ in 0..4 {
        step(&scheduler).await;
    }
    assert_eq!(executor.rerun_requests.lock().unwrap().len(), 7);
    assert_eq!(pr_of(&store, id).reruns, 7);
    let comments = executor.comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].1.contains("7 reruns"));
    let Turn::User(Ask::Renovate { reason, .. }) = store.get(id).unwrap().job.turn().clone() else {
        panic!("the job should wait for the user");
    };
    assert!(reason.contains("7 reruns"));

    *executor.renovate_now.lock().unwrap() = failing("h2");
    step_until(&scheduler, || {
        pr_of(&store, id).head.as_deref() == Some("h2")
    })
    .await;
    assert!(pr_of(&store, id).reruns <= 1);

    *executor.renovate_now.lock().unwrap() = failing("h1");
    set_limits(&store, 3, 4);
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    assert_eq!(pr_of(&store, id).reruns, 4, "the saved total of 4 applies");

    let before = executor.rerun_requests.lock().unwrap().len();
    store.apply(id, Event::Retry).unwrap();
    step(&scheduler).await;
    assert_eq!(executor.rerun_requests.lock().unwrap().len(), before + 1);
    assert_eq!(pr_of(&store, id).reruns, 0);
    assert_eq!(pr_of(&store, id).auto_reruns, 0);
}

#[tokio::test]
async fn a_lowered_total_stops_a_job_that_is_already_past_it() {
    let (store, executor, scheduler, id) = renovate_setup(transient_runs);
    *executor.renovate_now.lock().unwrap() = failing("h1");
    step_until(&scheduler, || pr_of(&store, id).reruns == 5).await;
    set_limits(&store, 3, 4);
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    assert_eq!(pr_of(&store, id).reruns, 5);
    for _ in 0..3 {
        step(&scheduler).await;
    }
    assert_eq!(pr_of(&store, id).reruns, 5, "no rerun after the stop");
}

#[tokio::test]
async fn a_safe_update_merges_only_with_auto_merge_on() {
    let (store, executor, scheduler, id) = renovate_setup(verdict(true));
    *executor.renovate_now.lock().unwrap() = passed("h1");
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    for _ in 0..2 {
        step(&scheduler).await;
    }
    assert_eq!(needs_you(&store, id), Some(RenovateWait::Ready));
    assert!(executor.merged.lock().unwrap().is_empty());
    assert_eq!(
        pr_of(&store, id).verdict.unwrap().summary,
        "Bumps serde to 1.0.2"
    );

    store.save_project_auto_merge("maestro", true).unwrap();
    step_until(&scheduler, || {
        *store.get(id).unwrap().job.stage() == Stage::Merged
    })
    .await;
    assert_eq!(*executor.merged.lock().unwrap(), [(7, "h1".to_owned())]);
    assert_eq!(
        executor.requests().len(),
        1,
        "no analysis run without packaging"
    );
}

#[tokio::test]
async fn an_unsafe_update_is_never_merged() {
    let (store, executor, scheduler, id) = renovate_setup(verdict(false));
    store.save_project_auto_merge("maestro", true).unwrap();
    *executor.renovate_now.lock().unwrap() = passed("h1");
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    for _ in 0..3 {
        step(&scheduler).await;
    }
    assert_eq!(needs_you(&store, id), Some(RenovateWait::Unsafe));
    assert!(executor.merged.lock().unwrap().is_empty());
    let comments = executor.comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].1.contains("it also edits the build script"));
}

#[tokio::test]
async fn a_pull_request_with_no_checks_is_not_merged() {
    let (store, executor, scheduler, id) = renovate_setup(verdict(true));
    store.save_project_auto_merge("maestro", true).unwrap();
    *executor.renovate_now.lock().unwrap() = RenovateNow::Ci("h1", RenovateCi::NoChecks);
    for _ in 0..3 {
        step(&scheduler).await;
    }
    assert!(executor.merged.lock().unwrap().is_empty());
    assert_eq!(executor.requests().len(), 0);
    assert_eq!(*store.get(id).unwrap().job.stage(), Stage::Renovate);
}

#[tokio::test]
async fn a_merge_by_a_person_finishes_the_job_and_a_close_cancels_it() {
    let (store, executor, scheduler, id) = renovate_setup(verdict(true));
    *executor.renovate_now.lock().unwrap() = RenovateNow::Merged;
    step(&scheduler).await;
    assert_eq!(*store.get(id).unwrap().job.stage(), Stage::Merged);

    let other = store
        .create_renovate_job("maestro", 8, "Update tokio", "https://example/pull/8")
        .unwrap()
        .unwrap();
    *executor.renovate_now.lock().unwrap() = RenovateNow::Closed;
    step(&scheduler).await;
    assert_eq!(*store.get(other).unwrap().job.stage(), Stage::Cancelled);
}

#[tokio::test]
async fn what_the_factory_posts_on_renovate_pull_requests_is_plain() {
    let (store, executor, scheduler, id) = renovate_setup(transient_runs);
    *executor.renovate_now.lock().unwrap() = failing("h1");
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    let mut bodies: Vec<String> = executor
        .comments
        .lock()
        .unwrap()
        .iter()
        .map(|(_, body)| body.clone())
        .collect();
    bodies.push(factory_server::APPROVAL.to_owned());
    assert!(bodies.len() > 1);
    for body in bodies {
        assert!(body.is_ascii(), "{body}");
        assert!(!body.contains("Moon Skywalker"));
    }
}

#[tokio::test]
async fn a_merge_refused_for_a_moved_head_watches_the_new_head() {
    let (store, executor, scheduler, id) = renovate_setup(verdict(true));
    store.save_project_auto_merge("maestro", true).unwrap();
    *executor.renovate_now.lock().unwrap() = passed("h1");
    executor
        .merge_failures
        .lock()
        .unwrap()
        .push_back(passed("h2"));
    step_until(&scheduler, || {
        *store.get(id).unwrap().job.stage() == Stage::Merged
    })
    .await;
    assert_eq!(*executor.merged.lock().unwrap(), [(7, "h2".to_owned())]);
    assert_eq!(executor.requests().len(), 2, "each head gets its own check");
    // The push landed after the watch of h1, so only h1 was approved then.
    assert_eq!(
        *executor.approved.lock().unwrap(),
        [(7, "h1".to_owned()), (7, "h2".to_owned())]
    );
}

#[tokio::test]
async fn a_merge_refused_for_a_pull_request_merged_by_a_person_ends_the_job() {
    let (store, executor, scheduler, id) = renovate_setup(verdict(true));
    store.save_project_auto_merge("maestro", true).unwrap();
    *executor.renovate_now.lock().unwrap() = passed("h1");
    executor
        .merge_failures
        .lock()
        .unwrap()
        .push_back(RenovateNow::Merged);
    step_until(&scheduler, || {
        *store.get(id).unwrap().job.stage() == Stage::Merged
    })
    .await;
    assert!(executor.merged.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failing_check_with_no_actions_run_is_not_counted_as_a_rerun() {
    let (store, executor, scheduler, id) = renovate_setup(transient_runs);
    executor
        .reruns
        .lock()
        .unwrap()
        .push_back(Ok(Rerun::NothingToRerun));
    *executor.renovate_now.lock().unwrap() = failing("h1");
    step_until(&scheduler, || needs_you(&store, id).is_some()).await;
    for _ in 0..4 {
        step(&scheduler).await;
    }
    assert_eq!(needs_you(&store, id), Some(RenovateWait::Stopped));
    let pr = pr_of(&store, id);
    assert_eq!((pr.reruns, pr.auto_reruns), (0, 0));
    assert_eq!(executor.rerun_requests.lock().unwrap().len(), 1);
    assert_eq!(executor.comments.lock().unwrap().len(), 1);
    assert_eq!(executor.requests().len(), 0);
}
