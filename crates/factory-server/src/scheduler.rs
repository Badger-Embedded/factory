use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use factory_core::{Command, Event, Stage};
use factory_runner::{Expect, LogLine, RunReport, prompt};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::settings::Settings;
use crate::store::{Store, StoreError};

/// How often the loop looks at the jobs when nothing wakes it.
const POLL: Duration = Duration::from_secs(5);

/// One agent run for the executor to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub job_id: i64,
    /// The Claude Code session UUID.
    pub session_id: String,
    pub expect: Expect,
    pub prompt: String,
}

pub type LogSink = Box<dyn FnMut(LogLine) + Send>;

/// Carries out commands that need a worker.
pub trait Executor: Send + Sync + 'static {
    /// Runs the agent to the end. Every failure comes back as
    /// [`Event::AgentFailed`] in the report.
    fn run_agent(
        &self,
        settings: &Settings,
        request: AgentRequest,
        log: LogSink,
    ) -> impl Future<Output = RunReport> + Send;

    /// Stops whatever runs for job `job_id`. Called when the job no longer
    /// wants the run, such as after a cancel.
    fn stop(&self, job_id: i64) -> impl Future<Output = ()> + Send;
}

/// Starts queued jobs and runs each job's next command, with at most
/// [`Settings::max_workers`] runs at a time.
///
/// What runs follows from the stored jobs alone, so after a restart the
/// scheduler picks up every job where it was.
pub struct Scheduler<E> {
    store: Arc<Store>,
    executor: Arc<E>,
    wake: Arc<Notify>,
    running: Mutex<HashMap<i64, JoinHandle<()>>>,
}

impl<E: Executor> Scheduler<E> {
    pub fn new(store: Arc<Store>, executor: Arc<E>, wake: Arc<Notify>) -> Arc<Self> {
        Arc::new(Scheduler {
            store,
            executor,
            wake,
            running: Mutex::new(HashMap::new()),
        })
    }

    /// Runs the loop forever. A notification on `wake` starts the next pass
    /// early.
    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(err) = self.tick().await {
                tracing::error!("job loop: {err}");
            }
            tokio::select! {
                () = self.wake.notified() => {}
                () = tokio::time::sleep(POLL) => {}
            }
        }
    }

    /// The jobs with a run in progress.
    #[must_use]
    pub fn running(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .lock()
            .iter()
            .filter(|(_, handle)| !handle.is_finished())
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// One pass over the jobs, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be read.
    pub async fn tick(self: &Arc<Self>) -> Result<(), StoreError> {
        let settings = self.store.settings()?;
        let mut rows = self.store.list()?;
        rows.reverse();

        let mut unwanted = Vec::new();
        {
            let mut running = self.lock();
            running.retain(|_, handle| !handle.is_finished());
            for row in &rows {
                let wants_agent = matches!(row.job.next_command(), Some(Command::RunAgent { .. }));
                if !wants_agent && let Some(handle) = running.remove(&row.id) {
                    handle.abort();
                    unwanted.push(row.id);
                }
            }
        }
        for id in unwanted {
            self.executor.stop(id).await;
            self.store.append_factory_log(
                id,
                "Stopped the agent run because the job no longer needs it.",
            )?;
        }

        for row in rows {
            let free = settings.max_workers.saturating_sub(self.lock().len());
            if free == 0 {
                break;
            }
            if self.lock().contains_key(&row.id) {
                continue;
            }
            let mut job = row.job;
            if *job.stage() == Stage::Requested {
                job = self.store.apply(row.id, Event::Start)?.job;
            }
            if let Some(Command::RunAgent { session, task }) = job.next_command()
                && let Some(expect) = Expect::for_stage(job.stage())
            {
                let request = AgentRequest {
                    job_id: row.id,
                    session_id: self.store.session_uuid(row.id, session)?,
                    expect,
                    prompt: prompt(&task, expect),
                };
                self.spawn(settings.clone(), request);
            }
        }
        Ok(())
    }

    fn spawn(self: &Arc<Self>, settings: Settings, request: AgentRequest) {
        let job_id = request.job_id;
        let scheduler = Arc::clone(self);
        let mut running = self.lock();
        let handle = tokio::spawn(async move {
            scheduler.carry_out(&settings, request).await;
            scheduler.lock().remove(&job_id);
            scheduler.wake.notify_one();
        });
        running.insert(job_id, handle);
    }

    async fn carry_out(&self, settings: &Settings, request: AgentRequest) {
        let job_id = request.job_id;
        let store = Arc::clone(&self.store);
        let note = |text: &str| {
            if let Err(err) = store.append_factory_log(job_id, text) {
                tracing::error!("job {job_id}: {err}");
            }
        };
        note(&format!(
            "Agent run started in session {}.",
            request.session_id
        ));

        let sink_store = Arc::clone(&self.store);
        let sink: LogSink = Box::new(move |line| {
            if let Err(err) = sink_store.append_log(job_id, &line) {
                tracing::error!("job {job_id}: {err}");
            }
        });
        let report = self.executor.run_agent(settings, request, sink).await;

        let cost = report
            .cost_usd
            .map_or(String::new(), |cost| format!(", ${cost:.2} at API prices"));
        note(&format!("Agent run finished{cost}."));
        match self.store.apply(job_id, report.event) {
            Ok(_) => {}
            Err(StoreError::Rejected(rejected)) => {
                note(&format!(
                    "The job did not take the run's result: {rejected}."
                ));
            }
            Err(err) => tracing::error!("job {job_id}: {err}"),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<i64, JoinHandle<()>>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
