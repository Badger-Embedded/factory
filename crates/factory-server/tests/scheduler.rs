use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::{
    AgentResult, Ask, BranchSummary, Event, Gates, PrDescription, Question, Stage, Turn,
};
use factory_runner::{Expect, LogLine, RunReport};
use factory_server::{
    AgentRequest, ChecksRequest, Executor, LogSink, Project, Projects, PublishRequest, Scheduler,
    Settings, Store, SummaryRequest, WatchReport, WatchRequest,
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
    stopped: Mutex<Vec<i64>>,
    checks: Mutex<VecDeque<Result<Event, String>>>,
    checks_run: Mutex<Vec<ChecksRequest>>,
    summarized: Mutex<Vec<SummaryRequest>>,
    publishes: Mutex<VecDeque<Result<u64, String>>>,
    published: Mutex<Vec<PublishRequest>>,
    watches: Mutex<VecDeque<Result<WatchReport, String>>>,
    watched: Mutex<Vec<WatchRequest>>,
}

impl FakeExecutor {
    fn new(respond: impl Fn(&AgentRequest) -> Event + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FakeExecutor {
            gate: Semaphore::new(0),
            respond: Box::new(respond),
            requests: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
            checks: Mutex::new(VecDeque::new()),
            checks_run: Mutex::new(Vec::new()),
            summarized: Mutex::new(Vec::new()),
            publishes: Mutex::new(VecDeque::new()),
            published: Mutex::new(Vec::new()),
            watches: Mutex::new(VecDeque::new()),
            watched: Mutex::new(Vec::new()),
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
            cost_usd: Some(0.12),
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

    async fn publish(&self, request: PublishRequest) -> Result<u64, String> {
        tokio::task::yield_now().await;
        self.published.lock().unwrap().push(request);
        self.publishes.lock().unwrap().pop_front().unwrap_or(Ok(12))
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
async fn answers_continue_the_session_and_a_pull_request_gets_its_own() {
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
        Expect::Commits => Event::Agent(AgentResult::CommitsReady(ready())),
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
    let planning = &requests[0].session_id;
    assert!(requests[..4].iter().all(|r| &r.session_id == planning));
    assert_ne!(&requests[4].session_id, planning);
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
        Stage::Failed { reason } if reason.contains("125")
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
        Stage::Failed { reason } if reason.contains("\"gone\"")
    ));
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
            auto_merge: true,
            ..Settings::default()
        })
        .unwrap();
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
