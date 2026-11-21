use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::{
    Agent, AgentResult, Ask, BranchSummary, DEFAULT_EFFORT, Event, Gates, MAX_FAILED_ROUNDS,
    MAX_REVIEW_ROUNDS, PrDescription, Question, ReviewState, Stage, Turn,
};
use factory_runner::{Expect, LogLine, McpUnavailable, Observation, Rerun, RunReport};
use factory_server::{
    AgentRequest, ChecksRequest, Executor, LogSink, McpProjectSettings, McpRuntime, McpStages,
    McpWarnings, Project, Projects, PublishRequest, Published, ReadyAction, RerunRequest,
    RunTokens, Scheduler, SecretRef, SecretStore, Settings, StageEfforts, Store, SummaryRequest,
    UnavailableSecrets, WatchReport, WatchRequest, format_cost, ready_action, watch_only_event,
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
    /// The MCP servers each run reports as not connected in its init line.
    unavailable: Mutex<VecDeque<Vec<McpUnavailable>>>,
    stopped: Mutex<Vec<i64>>,
    checks: Mutex<VecDeque<Result<Event, String>>>,
    checks_run: Mutex<Vec<ChecksRequest>>,
    summarized: Mutex<Vec<SummaryRequest>>,
    publishes: Mutex<VecDeque<Result<Published, String>>>,
    published: Mutex<Vec<PublishRequest>>,
    watches: Mutex<VecDeque<Result<WatchReport, String>>>,
    watched: Mutex<Vec<WatchRequest>>,
    reruns: Mutex<VecDeque<Result<Rerun, String>>>,
    rerun_requests: Mutex<Vec<RerunRequest>>,
}

impl FakeExecutor {
    fn new(respond: impl Fn(&AgentRequest) -> Event + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FakeExecutor {
            gate: Semaphore::new(0),
            respond: Box::new(respond),
            requests: Mutex::new(Vec::new()),
            costs: Mutex::new(VecDeque::new()),
            unavailable: Mutex::new(VecDeque::new()),
            stopped: Mutex::new(Vec::new()),
            checks: Mutex::new(VecDeque::new()),
            checks_run: Mutex::new(Vec::new()),
            summarized: Mutex::new(Vec::new()),
            publishes: Mutex::new(VecDeque::new()),
            published: Mutex::new(Vec::new()),
            watches: Mutex::new(VecDeque::new()),
            watched: Mutex::new(Vec::new()),
            reruns: Mutex::new(VecDeque::new()),
            rerun_requests: Mutex::new(Vec::new()),
        })
    }

    fn release(&self, runs: usize) {
        self.gate.add_permits(runs);
    }

    /// Makes the next run report `servers` as not connected in its init line,
    /// the way a worker's stream-json `init` line does.
    fn report_unavailable(&self, servers: Vec<McpUnavailable>) {
        self.unavailable.lock().unwrap().push_back(servers);
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
    ) -> RunReport {
        self.requests.lock().unwrap().push(request.clone());
        self.gate.acquire().await.unwrap().forget();
        log(LogLine::Text("Reading the repository.".into()));
        RunReport {
            event: (self.respond)(&request),
            session_id: Some(request.session_id),
            cost_usd: self.costs.lock().unwrap().pop_front().unwrap_or(Some(0.12)),
            mcp_unavailable: self
                .unavailable
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default(),
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

    async fn stop(&self, job_id: i64) {
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
        }])),
        Expect::Commits | Expect::FixCi => Event::Agent(AgentResult::CommitsReady(ready())),
        Expect::Review => unreachable!("this job keeps llm review off, so no reviewer runs"),
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
async fn costs_sum_over_the_runs_of_a_job() {
    let (store, _scheduler, id) = two_clarify_runs([Some(0.5), Some(1.25)]).await;

    assert!((store.cost_usd(id).unwrap() - 1.75).abs() < 1e-9);
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

fn planned_pr(title: &str) -> factory_core::PlannedPr {
    factory_core::PlannedPr {
        title: title.into(),
        summary: "s".into(),
        number: None,
        description: None,
    }
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
    store.add_cost(first, 0.5).unwrap();
    store.add_cost(first, 0.25).unwrap();
    store.add_cost(second, 1.0).unwrap();
    store.add_cost(other, 4.0).unwrap();

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
    store.add_cost(id, 0.75).unwrap();
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
                },
                factory_core::PlannedPr {
                    title: "web: add a reboot button".into(),
                    summary: "Calls it.".into(),
                    number: None,
                    description: None,
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
    green(&executor, Event::StartReview);
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

    green(&executor, Event::StartReview);
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

    green(&executor, Event::StartReview);
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
    green(&executor, Event::StartReview);
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

    green(&executor, Event::StartReview);
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

    green(&executor, Event::StartReview);
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
        green(&executor, Event::StartReview);
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
        f.store.add_cost(id, 0.5).unwrap();
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
        f.store.add_cost(id, 0.25).unwrap();
        f.finish(id, "merged");
        f.scheduler.tick().await.unwrap();

        assert!(!f.has_work(id));
        assert!(!f.has_claude(id));
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
        }])),
        Expect::Commits | Expect::FixCi => Event::Agent(AgentResult::CommitsReady(ready())),
        Expect::Review => unreachable!("this job keeps llm review off, so no reviewer runs"),
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
    assert_eq!(used, [DEFAULT_EFFORT; 5]);
}

#[tokio::test]
async fn each_stage_runs_with_its_saved_effort() {
    let (store, executor, scheduler) = setup(one_pass);
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("low", "high", "xhigh", "max", "medium"),
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
        .save_project_efforts("maestro", &stage_efforts("low", "low", "low", "low", "low"))
        .unwrap();
    let id = store.create("maestro", "alice", "Add a button").unwrap();
    executor.release(10);

    step(&scheduler).await;
    store
        .save_project_efforts(
            "maestro",
            &stage_efforts("high", "high", "high", "high", "high"),
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
        )
        .unwrap();
    to_watching(&store, &executor, &scheduler).await;

    green(&executor, Event::StartReview);
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

    // Drive the review run for the same pull request.
    green(&executor, Event::StartReview);
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
    // The server was skipped, so the run carries no MCP config.
    assert!(executor.requests()[0].mcp.is_none());
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
