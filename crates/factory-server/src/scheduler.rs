use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use factory_core::{BranchSummary, Command, Event, Job, PrDescription, Session, Stage};
use factory_runner::{Expect, LogLine, RunReport, job_prompt};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cost::format_cost;
use crate::projects::{Project, Projects};
use crate::settings::Settings;
use crate::store::{JobRow, Store, StoreError};

/// How often the loop looks at the jobs when nothing wakes it.
const POLL: Duration = Duration::from_secs(5);

/// A finished job is deleted once it is this many days old, counted from
/// `created_at`.
pub const FINISHED_JOB_DAYS: u32 = 30;

/// The branch the factory pushes planned pull request `pr` of job `job_id`
/// from.
#[must_use]
pub fn branch_name(job_id: i64, pr: usize) -> String {
    format!("factory/job-{job_id}-pr-{}", pr + 1)
}

/// One agent run for the executor to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub job_id: i64,
    /// The GitHub repository as `owner/name`.
    pub repo: String,
    /// The branch pull requests target.
    pub base: String,
    /// The pull request branch, or `None` for a planning run, which reads
    /// the base branch.
    pub branch: Option<String>,
    /// The Claude Code session UUID.
    pub session_id: String,
    pub expect: Expect,
    pub prompt: String,
    /// The model for the run's stage, or `None` when the project sets none.
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksRequest {
    pub job_id: i64,
    /// The branch the pull request targets.
    pub base: String,
    /// The project's `checks` command.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRequest {
    pub job_id: i64,
    /// The branch the pull request targets.
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRequest {
    pub job_id: i64,
    pub repo: String,
    pub base: String,
    pub branch: String,
    pub description: PrDescription,
}

/// What a publish found on GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// The branch was pushed and this open pull request has its description.
    Open(u64),
    /// This pull request for the branch was merged before the push, so the
    /// factory pushed nothing.
    AlreadyMerged(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRequest {
    pub job_id: i64,
    pub repo: String,
    /// The branch the pull request targets.
    pub base: String,
    pub number: u64,
    /// The time of the newest comment the agent already has.
    pub seen_until: Option<String>,
    /// Merge the pull request when it is ready.
    pub auto_merge: bool,
}

/// What a look at a pull request found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchReport {
    /// `None` while CI runs and nobody commented.
    pub event: Option<Event>,
    /// The new comment watermark, when `event` delivers comments.
    pub seen_until: Option<String>,
    /// The executor merged the pull request because it was ready and
    /// auto-merge is on.
    pub merged_by_factory: bool,
}

pub type LogSink = Box<dyn FnMut(LogLine) + Send>;

/// Carries out the commands of [`factory_core::Command`].
///
/// An `Err` from a method means the factory could not do the step, such as
/// GitHub being unreachable. The job stays where it is and the scheduler
/// tries again after [`Settings::poll_seconds`].
pub trait Executor: Send + Sync + 'static {
    /// Runs the agent to the end. Every failure comes back as
    /// [`Event::AgentFailed`] in the report.
    fn run_agent(
        &self,
        settings: &Settings,
        request: AgentRequest,
        log: LogSink,
    ) -> impl Future<Output = RunReport> + Send;

    /// Runs the project's checks in a worker.
    fn run_checks(
        &self,
        settings: &Settings,
        request: ChecksRequest,
    ) -> impl Future<Output = Result<Event, String>> + Send;

    /// Lists the commits and the diff stat the branch adds to its base.
    fn summarize(
        &self,
        request: SummaryRequest,
    ) -> impl Future<Output = Result<BranchSummary, String>> + Send;

    /// Pushes the branch and opens or updates its pull request. When a pull
    /// request for the branch is already merged, it pushes nothing and
    /// reports that pull request instead.
    fn publish(
        &self,
        request: PublishRequest,
    ) -> impl Future<Output = Result<Published, String>> + Send;

    fn watch(
        &self,
        request: WatchRequest,
    ) -> impl Future<Output = Result<WatchReport, String>> + Send;

    /// Stops whatever runs for job `job_id`. Called when the job no longer
    /// wants the run, such as after a cancel.
    fn stop(&self, job_id: i64) -> impl Future<Output = ()> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Holds one of the [`Settings::max_workers`] worker slots.
    Worker,
    /// Runs in the factory process.
    Factory,
}

struct Task {
    kind: Kind,
    handle: JoinHandle<()>,
}

/// Starts queued jobs and carries out each job's next command, with at
/// most [`Settings::max_workers`] workers at a time.
///
/// What runs follows from the stored jobs alone, so after a restart the
/// scheduler picks up every job where it was.
pub struct Scheduler<E> {
    store: Arc<Store>,
    projects: Arc<Projects>,
    executor: Arc<E>,
    wake: Arc<Notify>,
    data_dir: PathBuf,
    tasks: Mutex<HashMap<i64, Task>>,
    /// Jobs whose factory step waits until the given time: the next look at
    /// a pull request, or a retry after a failed step.
    not_before: Mutex<HashMap<i64, Instant>>,
}

impl<E: Executor> Scheduler<E> {
    pub fn new(
        store: Arc<Store>,
        projects: Arc<Projects>,
        executor: Arc<E>,
        wake: Arc<Notify>,
        data_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Scheduler {
            store,
            projects,
            executor,
            wake,
            data_dir,
            tasks: Mutex::new(HashMap::new()),
            not_before: Mutex::new(HashMap::new()),
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

    /// The jobs with a step in progress.
    #[must_use]
    pub fn running(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .tasks()
            .iter()
            .filter(|(_, task)| !task.handle.is_finished())
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
        self.stop_unwanted(&rows).await?;
        let rows = self.purge_expired(rows)?;

        for row in rows {
            if self.tasks().contains_key(&row.id) {
                continue;
            }
            let mut job = row.job.clone();
            let workers_free = self.workers_free(&settings);
            if *job.stage() == Stage::Requested {
                if !workers_free {
                    continue;
                }
                job = self.store.apply(row.id, Event::Start)?.job;
            }
            let Some(command) = job.next_command() else {
                continue;
            };
            let Some(project) = self.projects.get(&row.project).cloned() else {
                self.fail_unknown_project(&row, &job)?;
                continue;
            };
            match command {
                Command::RunAgent { session, task } if workers_free => {
                    let Some(expect) = Expect::for_stage(job.stage()) else {
                        continue;
                    };
                    let models = self.store.project_models(&row.project, &project.models)?;
                    let model = match expect {
                        Expect::Clarify => models.clarify,
                        Expect::Spec => models.spec,
                        Expect::Plan => models.plan,
                        Expect::Commits => models.implement,
                    };
                    let request = AgentRequest {
                        job_id: row.id,
                        repo: project.repo.clone(),
                        base: project.base.clone(),
                        branch: match session {
                            Session::Planning | Session::Spec | Session::Plan => None,
                            Session::Pr(pr) => Some(branch_name(row.id, pr)),
                        },
                        session_id: self.store.session_uuid(row.id, session)?,
                        expect,
                        prompt: job_prompt(&job, &task, expect),
                        model,
                    };
                    let scheduler = Arc::clone(self);
                    let settings = settings.clone();
                    self.spawn(row.id, Kind::Worker, async move {
                        scheduler.agent(&settings, request).await;
                    });
                }
                Command::RunChecks { .. } => {
                    self.checks(row.id, &project, &settings, workers_free)?;
                }
                Command::Summarize { .. } if self.due(row.id) => {
                    self.summarize(row.id, &project, &settings);
                }
                Command::Publish { pr } if self.due(row.id) => {
                    self.publish(row.id, pr, &job, &project, &settings)?;
                }
                Command::WatchPr { pr, number } if self.due(row.id) => {
                    let request = WatchRequest {
                        job_id: row.id,
                        repo: project.repo.clone(),
                        base: project.base.clone(),
                        number,
                        seen_until: self.store.pr_watermark(row.id, pr)?,
                        auto_merge: settings.auto_merge,
                    };
                    let scheduler = Arc::clone(self);
                    let settings = settings.clone();
                    self.spawn(row.id, Kind::Factory, async move {
                        scheduler.watch(&settings, pr, request).await;
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Deletes the finished jobs older than [`FINISHED_JOB_DAYS`] and returns
    /// the rows that remain. A job that fails to delete is logged and kept.
    fn purge_expired(&self, rows: Vec<JobRow>) -> Result<Vec<JobRow>, StoreError> {
        let old = self.store.ids_older_than(FINISHED_JOB_DAYS)?;
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if row.job.is_finished() && old.contains(&row.id) {
                match self.store.delete_job(row.id, &self.data_dir) {
                    Ok(()) => continue,
                    Err(err) => tracing::error!("deleting job {}: {err}", row.id),
                }
            }
            kept.push(row);
        }
        Ok(kept)
    }

    async fn stop_unwanted(&self, rows: &[JobRow]) -> Result<(), StoreError> {
        let mut unwanted = Vec::new();
        {
            let mut tasks = self.tasks();
            tasks.retain(|_, task| !task.handle.is_finished());
            for row in rows {
                if row.job.next_command().is_none()
                    && let Some(task) = tasks.remove(&row.id)
                {
                    task.handle.abort();
                    unwanted.push(row.id);
                }
            }
        }
        for id in unwanted {
            self.executor.stop(id).await;
            self.store.append_factory_log(
                id,
                "Stopped the running step because the job no longer needs it.",
            )?;
        }
        Ok(())
    }

    fn workers_free(&self, settings: &Settings) -> bool {
        let busy = self
            .tasks()
            .values()
            .filter(|task| task.kind == Kind::Worker && !task.handle.is_finished())
            .count();
        busy < settings.max_workers
    }

    fn fail_unknown_project(&self, row: &JobRow, job: &Job) -> Result<(), StoreError> {
        let reason = format!("project {:?} is not configured", row.project);
        if matches!(job.next_command(), Some(Command::RunAgent { .. })) {
            self.store.apply(row.id, Event::AgentFailed { reason })?;
        } else {
            self.store.append_factory_log(row.id, &reason)?;
        }
        Ok(())
    }

    fn checks(
        self: &Arc<Self>,
        job_id: i64,
        project: &Project,
        settings: &Settings,
        workers_free: bool,
    ) -> Result<(), StoreError> {
        let Some(command) = project.checks.clone() else {
            self.store.append_factory_log(
                job_id,
                "The project configures no checks, so the factory pushes without running any.",
            )?;
            self.store.apply(job_id, Event::ChecksPassed)?;
            self.wake.notify_one();
            return Ok(());
        };
        if !workers_free || !self.due(job_id) {
            return Ok(());
        }
        let scheduler = Arc::clone(self);
        let settings = settings.clone();
        let base = project.base.clone();
        self.spawn(job_id, Kind::Worker, async move {
            scheduler.note(job_id, &format!("Running the checks: {command}"));
            let result = scheduler
                .executor
                .run_checks(
                    &settings,
                    ChecksRequest {
                        job_id,
                        base,
                        command,
                    },
                )
                .await;
            match result {
                Ok(event) => {
                    let passed = event == Event::ChecksPassed;
                    scheduler.note(
                        job_id,
                        if passed {
                            "The checks passed."
                        } else {
                            "The checks failed."
                        },
                    );
                    scheduler.apply(job_id, event);
                }
                Err(err) => scheduler.retry_later(&settings, job_id, &err),
            }
        });
        Ok(())
    }

    fn summarize(self: &Arc<Self>, job_id: i64, project: &Project, settings: &Settings) {
        let request = SummaryRequest {
            job_id,
            base: project.base.clone(),
        };
        let scheduler = Arc::clone(self);
        let settings = settings.clone();
        self.spawn(job_id, Kind::Factory, async move {
            match scheduler.executor.summarize(request).await {
                Ok(summary) => scheduler.apply(job_id, Event::Summarized(summary)),
                Err(err) => scheduler.retry_later(&settings, job_id, &err),
            }
        });
    }

    fn publish(
        self: &Arc<Self>,
        job_id: i64,
        pr: usize,
        job: &Job,
        project: &Project,
        settings: &Settings,
    ) -> Result<(), StoreError> {
        let Some(description) = job.plan()[pr].description.clone() else {
            self.store.append_factory_log(
                job_id,
                "The agent wrote no pull request description, so there is nothing to publish.",
            )?;
            return Ok(());
        };
        let request = PublishRequest {
            job_id,
            repo: project.repo.clone(),
            base: project.base.clone(),
            branch: branch_name(job_id, pr),
            description,
        };
        let scheduler = Arc::clone(self);
        let settings = settings.clone();
        self.spawn(job_id, Kind::Factory, async move {
            scheduler.note(
                job_id,
                &format!("Pushing {} to {}.", request.branch, request.repo),
            );
            match scheduler.executor.publish(request).await {
                Ok(Published::Open(number)) => {
                    scheduler.note(job_id, &format!("Pull request #{number} is up to date."));
                    scheduler.apply(job_id, Event::Published { number });
                }
                Ok(Published::AlreadyMerged(number)) => {
                    scheduler.note(
                        job_id,
                        &format!("Pull request #{number} was merged before the push, so the factory pushed nothing."),
                    );
                    scheduler.apply(job_id, Event::Published { number });
                    scheduler.apply(job_id, Event::Merged);
                }
                Err(err) => scheduler.retry_later(&settings, job_id, &err),
            }
        });
        Ok(())
    }

    async fn agent(&self, settings: &Settings, request: AgentRequest) {
        let job_id = request.job_id;
        self.note(
            job_id,
            &format!("Agent run started in session {}.", request.session_id),
        );
        let sink_store = Arc::clone(&self.store);
        let sink: LogSink = Box::new(move |line| {
            if let Err(err) = sink_store.append_log(job_id, &line) {
                tracing::error!("job {job_id}: {err}");
            }
        });
        let report = self.executor.run_agent(settings, request, sink).await;
        if let Some(usd) = report.cost_usd
            && let Err(err) = self.store.add_cost(job_id, usd)
        {
            tracing::error!("job {job_id}: {err}");
        }
        let cost = report.cost_usd.map_or(String::new(), |usd| {
            format!(", {} at API prices", format_cost(usd))
        });
        self.note(job_id, &format!("Agent run finished{cost}."));
        self.apply(job_id, report.event);
    }

    async fn watch(&self, settings: &Settings, pr: usize, request: WatchRequest) {
        let job_id = request.job_id;
        let number = request.number;
        self.wait(settings, job_id);
        match self.executor.watch(request).await {
            Ok(WatchReport {
                event: Some(event),
                seen_until,
                merged_by_factory,
            }) => {
                if merged_by_factory {
                    self.note(
                        job_id,
                        &format!("Merged #{number}: its checks passed and auto-merge is on."),
                    );
                }
                let result = match &seen_until {
                    Some(seen) => self.store.apply_with_watermark(job_id, pr, event, seen),
                    None => self.store.apply(job_id, event),
                };
                self.report(job_id, result.map(|_| ()));
            }
            Ok(WatchReport { event: None, .. }) => {}
            Err(err) => self.retry_later(settings, job_id, &err),
        }
    }

    fn apply(&self, job_id: i64, event: Event) {
        let result = self.store.apply(job_id, event).map(|_| ());
        self.report(job_id, result);
    }

    fn report(&self, job_id: i64, result: Result<(), StoreError>) {
        match result {
            Ok(()) => {}
            Err(StoreError::Rejected(rejected)) => {
                self.note(
                    job_id,
                    &format!("The job did not take the result: {rejected}."),
                );
            }
            Err(err) => tracing::error!("job {job_id}: {err}"),
        }
    }

    fn retry_later(&self, settings: &Settings, job_id: i64, err: &str) {
        self.wait(settings, job_id);
        self.note(
            job_id,
            &format!(
                "{err}. The factory tries again in {} seconds.",
                settings.poll_seconds
            ),
        );
    }

    fn wait(&self, settings: &Settings, job_id: i64) {
        let until = Instant::now() + Duration::from_secs(settings.poll_seconds);
        self.not_before
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(job_id, until);
    }

    fn due(&self, job_id: i64) -> bool {
        self.not_before
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&job_id)
            .is_none_or(|until| Instant::now() >= *until)
    }

    fn note(&self, job_id: i64, text: &str) {
        if let Err(err) = self.store.append_factory_log(job_id, text) {
            tracing::error!("job {job_id}: {err}");
        }
    }

    fn spawn(
        self: &Arc<Self>,
        job_id: i64,
        kind: Kind,
        work: impl Future<Output = ()> + Send + 'static,
    ) {
        let scheduler = Arc::clone(self);
        let mut tasks = self.tasks();
        let handle = tokio::spawn(async move {
            work.await;
            scheduler.tasks().remove(&job_id);
            scheduler.wake.notify_one();
        });
        tasks.insert(job_id, Task { kind, handle });
    }

    fn tasks(&self) -> MutexGuard<'_, HashMap<i64, Task>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
