use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::{AgentResult, Event, Question, Stage, Turn};
use factory_runner::{Expect, LogLine, RunReport};
use factory_server::{
    AgentRequest, Executor, LogSink, Project, Projects, Scheduler, Settings, Store,
};
use tokio::sync::{Notify, Semaphore};

type Respond = Box<dyn Fn(&AgentRequest) -> Event + Send + Sync>;

/// Holds each run until the test adds a permit to `gate`, then answers with
/// `respond`.
struct FakeExecutor {
    gate: Semaphore,
    respond: Respond,
    requests: Mutex<Vec<AgentRequest>>,
    stopped: Mutex<Vec<i64>>,
}

impl FakeExecutor {
    fn new(respond: impl Fn(&AgentRequest) -> Event + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FakeExecutor {
            gate: Semaphore::new(0),
            respond: Box::new(respond),
            requests: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
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
        },
    )]
    .into_iter()
    .collect();
    let scheduler = Scheduler::new(
        Arc::clone(&store),
        Arc::new(projects),
        Arc::clone(&executor),
        Arc::new(Notify::new()),
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
        }])),
        Expect::Commits => Event::Agent(AgentResult::CommitsReady),
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
    assert!(last.text.contains("Stopped the agent run"), "{}", last.text);
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
