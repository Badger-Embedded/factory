use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::{
    AgentResult, Ask, BranchSummary, Event, Gates, MAX_FAILED_ROUNDS, PrDescription, Question,
    Stage, Turn,
};
use factory_runner::{Expect, LogLine, Rerun, RunReport};
use factory_server::{
    AgentRequest, ChecksRequest, Executor, LogSink, Project, Projects, PublishRequest, Published,
    RerunRequest, Scheduler, Settings, Store, SummaryRequest, WatchReport, WatchRequest,
    format_cost,
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
    );
    (store, executor, scheduler)
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
                },
            )]
            .into_iter()
            .collect(),
        ),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
        PathBuf::new(),
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
        /// Creates a job with a log line, an image row, a session, an image
        /// file and a work directory, created `days` days ago.
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
                .session_uuid(id, factory_core::Session::Planning)
                .unwrap();
            self.store.add_images(id, &["image-1.png".into()]).unwrap();
            let images = self.dir(id).join("images");
            std::fs::create_dir_all(&images).unwrap();
            std::fs::write(images.join("image-1.png"), b"png").unwrap();
            std::fs::create_dir_all(self.dir(id).join("work")).unwrap();
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

        for table in ["logs", "sessions", "job_images"] {
            assert_eq!(f.rows(table, old), 0, "{table}");
            assert_eq!(f.rows(table, other), 1, "{table}");
        }
        assert!(!f.dir(old).exists());
        assert!(f.dir(other).join("images/image-1.png").exists());
        assert_eq!(f.store.images(old).unwrap(), Vec::<String>::new());
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
