use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use factory_core::{
    Agent, BranchSummary, Command, DEFAULT_AGENT, Event, Job, PrDescription, ReviewState, Session,
    Stage,
};
use factory_runner::{Expect, LogLine, McpConfig, Observation, Rerun, RunReport, job_prompt};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::cost::{codex_cost, format_cost};
use crate::mcp::McpServer;
use crate::projects::{Project, Projects, StageAgents};
use crate::proxy::RunTokens;
use crate::secrets::{SecretError, SecretRef, SecretStore};
use crate::settings::Settings;
use crate::store::{JobRow, Store, StoreError};
use crate::warnings::McpWarnings;
use crate::workers::{WorkerSlots, unix_now};

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

/// The agent a run in `expect` reads from `agents`, with the same stage mapping
/// as the model and the effort: the check fix and CI fix runs use the implement
/// agent. A stored id always names an available agent; a stray one falls back to
/// the default rather than failing the run.
fn stage_agent(agents: &StageAgents, expect: Expect) -> Agent {
    let id = match expect {
        Expect::Clarify => &agents.clarify,
        Expect::Spec => &agents.spec,
        Expect::Plan => &agents.plan,
        Expect::Commits | Expect::FixCi => &agents.implement,
        Expect::Review => &agents.review,
    };
    Agent::from_id(id).unwrap_or(DEFAULT_AGENT)
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
    /// The agent session UUID the factory picks. Claude passes it to
    /// `--session-id` and `--resume`; Codex, which names its own session, uses it
    /// only as the key its reported id is stored under.
    pub session_id: String,
    /// The id the agent reported for this session on an earlier run, such as
    /// Codex's thread id, or `None` for a session that has not reported one. The
    /// runner resumes Codex on it when its rollout file exists and otherwise
    /// starts fresh. Claude leaves it `None` and resumes by `session_id`.
    pub agent_session: Option<String>,
    /// The agent the run uses. The scheduler picks the stage's agent for a fresh
    /// session and keeps a resumed session on the agent it started, see
    /// [`Store::session_uuid`](crate::Store::session_uuid).
    pub agent: Agent,
    pub expect: Expect,
    pub prompt: String,
    /// The model for the run's stage, or `None` when the project sets none.
    pub model: Option<String>,
    /// The effort for the run's stage. The scheduler reads the project's saved
    /// effort on every run and always fills this in, with the default when the
    /// project saved none. `None` is reserved for the runner, which then uses
    /// its own default, so an older server that sends nothing still runs.
    pub effort: Option<String>,
    /// The MCP servers active for this run's project and stage, with the per-run
    /// proxy token, or `None` when none is active. The runner passes it to
    /// `claude --mcp-config`.
    pub mcp: Option<McpConfig>,
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
    /// The branch has no commits after the base branch, so the factory
    /// pushed nothing and opened no pull request.
    Empty,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact about the pull request or the watch mode"
)]
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
    /// Run the LLM review before any merge. When it is on and the current head
    /// is not yet approved, a ready pull request starts a review instead of
    /// merging or waiting.
    pub llm_review: bool,
    /// The reviewer has approved the current head. A ready pull request then
    /// merges or waits for the user without another review pass.
    pub review_approved: bool,
    /// Report only a merge or close by someone else, and take no other action.
    /// Set while the review has stopped and waits for the user: the watch does
    /// not merge, start a review, or send CI failures and comments to the
    /// agent, and it leaves the comment watermark where it is.
    pub watch_only: bool,
}

/// What the factory does with a pull request once its checks are green. The
/// review runs before any merge, so a reviewing project whose current head is
/// not yet approved reviews first. Pure, so the wiring is testable without
/// podman or GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyAction {
    /// Start the LLM review of the current head.
    Review,
    /// Squash-merge the pull request.
    Merge,
    /// Leave the pull request for the user to merge on GitHub.
    Wait,
}

/// Decides [`ReadyAction`] from the project's flags and the head's review
/// state. The review comes first: a reviewing project reviews an unapproved
/// head before it merges or waits.
#[must_use]
pub fn ready_action(llm_review: bool, review_approved: bool, auto_merge: bool) -> ReadyAction {
    if llm_review && !review_approved {
        ReadyAction::Review
    } else if auto_merge {
        ReadyAction::Merge
    } else {
        ReadyAction::Wait
    }
}

/// The event a watch reports while the review has stopped and waits for the
/// user. It reacts only to a merge or close by someone else: the job moves on
/// when the user merges the pull request on GitHub, and fails when they close
/// it. Every other observation (green, CI failed, conflict, new comments,
/// nothing) returns `None`, so the watch takes no action and never merges or
/// restarts the reviewer on its own. Pure, so the wiring is testable without
/// podman or GitHub.
#[must_use]
pub fn watch_only_event(observation: &Observation) -> Option<Event> {
    match observation {
        Observation::Merged => Some(Event::Merged),
        Observation::Closed => Some(Event::ClosedUnmerged),
        Observation::Conflicted
        | Observation::CiFailed(_)
        | Observation::Comments { .. }
        | Observation::Ready
        | Observation::Nothing => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerunRequest {
    pub job_id: i64,
    pub repo: String,
    pub number: u64,
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

/// Takes the id an agent reports for its own session, such as Codex's thread
/// id, the moment the run reports it, so the scheduler stores it before the run
/// ends and a run that dies after that line still resumes.
pub type SessionSink = Box<dyn FnMut(String) + Send>;

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
        session: SessionSink,
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
    /// reports that pull request instead. When the branch has no commits
    /// after the base branch, it pushes nothing and reports
    /// [`Published::Empty`].
    fn publish(
        &self,
        request: PublishRequest,
    ) -> impl Future<Output = Result<Published, String>> + Send;

    fn watch(
        &self,
        request: WatchRequest,
    ) -> impl Future<Output = Result<WatchReport, String>> + Send;

    /// Reruns the failed CI runs of the pull request. It returns after
    /// GitHub stops reporting the old failure, so the next watch does not
    /// count it again.
    fn rerun(&self, request: RerunRequest) -> impl Future<Output = Result<Rerun, String>> + Send;

    /// Stops whatever runs for job `job_id`. Called when the job no longer
    /// wants the run, such as after a cancel.
    fn stop(&self, job_id: i64) -> impl Future<Output = ()> + Send;
}

/// What the scheduler needs to give a run its MCP servers: the token registry
/// the proxy honours, the secret store it reads to skip a server whose stored
/// value cannot be decrypted, and the proxy base URL a worker reaches. Held
/// together so [`Scheduler::new`] takes one argument, and so a build with no
/// proxy can pass an empty registry and an unreachable URL.
pub struct McpRuntime {
    /// The per-run tokens the proxy honours. The scheduler issues one per run
    /// with an active server and revokes it when the worker exits.
    pub tokens: Arc<RunTokens>,
    /// The secret store, read to skip a server whose stored header value does
    /// not decrypt, so a broken credential does not reach a worker's config.
    pub secrets: Arc<dyn SecretStore>,
    /// The proxy's base URL a worker reaches, such as `http://factory:8081`.
    pub proxy_url: String,
    /// Records an unavailable MCP server as a warning on the job. The scheduler
    /// reports a server it skipped for an undecryptable secret and the servers
    /// the runner read as not connected; the proxy shares this recorder too, so
    /// a server is warned about at most once per run.
    pub warnings: Arc<McpWarnings>,
}

/// A server left out of a run because a stored header value would not decrypt,
/// with the reason the run records as its warning.
type SkippedServer = (String, String);

/// The MCP servers chosen for one agent run: the config the worker gets, and
/// the servers left out because a stored header value would not decrypt, each
/// with a reason the run records as a warning.
struct McpSelection {
    config: Option<McpConfig>,
    skipped: Vec<SkippedServer>,
}

/// The servers active for a run and the servers skipped for an undecryptable
/// secret, as [`Scheduler::active_mcp_servers`] sorts them.
struct ActiveServers {
    active: Vec<String>,
    skipped: Vec<SkippedServer>,
}

/// Revokes a run's proxy token when the run ends, by success, failure or
/// cancel. A cancel aborts the run's task, which drops its future, so `Drop` is
/// the only point that fires in every case. A factory restart drops the whole
/// token registry, so this only has to cover a live run.
struct TokenGuard {
    tokens: Arc<RunTokens>,
    token: String,
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        self.tokens.revoke(&self.token);
    }
}

/// Clears a run's MCP warning dedup state when the run ends, by success,
/// failure or cancel. A cancel aborts the run's task and drops its future, so
/// `Drop` is the only point that fires in every case, like [`TokenGuard`].
struct WarnGuard {
    warnings: Arc<McpWarnings>,
    job_id: i64,
}

impl Drop for WarnGuard {
    fn drop(&mut self) {
        self.warnings.end(self.job_id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Holds one of the [`Settings::max_workers`] worker slots.
    Worker,
    /// Runs in the factory process.
    Factory,
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
    /// Jobs whose factory step waits until the given time: the next look at
    /// a pull request, or a retry after a failed step.
    not_before: Mutex<HashMap<i64, Instant>>,
    slots: Arc<WorkerSlots>,
    mcp: McpRuntime,
}

impl<E: Executor> Scheduler<E> {
    pub fn new(
        store: Arc<Store>,
        projects: Arc<Projects>,
        executor: Arc<E>,
        wake: Arc<Notify>,
        data_dir: PathBuf,
        mcp: McpRuntime,
    ) -> Arc<Self> {
        Arc::new(Scheduler {
            store,
            projects,
            executor,
            wake,
            data_dir,
            not_before: Mutex::new(HashMap::new()),
            slots: Arc::new(WorkerSlots::new()),
            mcp,
        })
    }

    /// The worker slots this scheduler fills, for the dashboard to show.
    #[must_use]
    pub fn slots(&self) -> Arc<WorkerSlots> {
        Arc::clone(&self.slots)
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
        self.slots.running()
    }

    /// One pass over the jobs, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be read.
    pub async fn tick(self: &Arc<Self>) -> Result<(), StoreError> {
        let _pass = self.slots.lock_pass().await;
        let settings = self.store.settings()?;
        let mut rows = self.store.list()?;
        rows.reverse();
        self.stop_unwanted(&rows).await?;
        let rows = self.purge_expired(rows)?;
        self.clean_merged(&rows)?;

        for row in rows {
            if self.slots.has_task(row.id) {
                continue;
            }
            let mut job = row.job.clone();
            let workers_free = self.workers_free(&settings);
            if *job.stage() == Stage::Requested {
                if !workers_free {
                    continue;
                }
                job = match self.store.apply(row.id, Event::Start) {
                    Ok(started) => started.job,
                    Err(StoreError::Rejected(_) | StoreError::NotFound(_)) => continue,
                    Err(err) => return Err(err),
                };
            }
            let Some(command) = job.next_command() else {
                continue;
            };
            let Some(project) = self.project_for(&row, &job)? else {
                continue;
            };
            match command {
                Command::RunAgent { session, task } if workers_free => {
                    let Some(expect) = Expect::for_task(job.stage(), &task) else {
                        continue;
                    };
                    let (request, skipped) =
                        self.agent_request(&row, &job, &project, session, &task, expect)?;
                    let scheduler = Arc::clone(self);
                    let settings = settings.clone();
                    self.spawn(row.id, Kind::Worker, async move {
                        scheduler.agent(&settings, request, skipped).await;
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
                watch @ Command::WatchPr { .. } if self.due(row.id) => {
                    self.watch_pr(&row, &job, &project, &settings, &watch)?;
                }
                Command::RerunCi { number, reason, .. } if self.due(row.id) => {
                    let request = RerunRequest {
                        job_id: row.id,
                        repo: project.repo.clone(),
                        number,
                    };
                    let scheduler = Arc::clone(self);
                    let settings = settings.clone();
                    self.spawn(row.id, Kind::Factory, async move {
                        scheduler.rerun(&settings, request, &reason).await;
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

    /// Frees disk by deleting each merged job's `work/` checkout and `claude/`
    /// session directory. It runs every pass, so it also cleans jobs merged
    /// before this change and retries a deletion that failed earlier. A merged
    /// job is final: nothing reads those directories again. It skips a job the
    /// scheduler still holds an unfinished task for, keeps everything else the
    /// job owns, logs the one line that reports the cleanup only on the pass
    /// that freed something, and on a deletion error logs the job id and goes
    /// on, the way [`Self::purge_expired`] does, so the next pass tries again.
    fn clean_merged(&self, rows: &[JobRow]) -> Result<(), StoreError> {
        for row in rows {
            if *row.job.stage() != Stage::Merged || self.slots.has_task(row.id) {
                continue;
            }
            match self.store.clean_merged_job(row.id, &self.data_dir) {
                Ok(true) => self.store.append_factory_log(
                    row.id,
                    "Removed the checkout and session files to free disk space.",
                )?,
                Ok(false) => {}
                Err(err) => tracing::error!("cleaning merged job {}: {err}", row.id),
            }
        }
        Ok(())
    }

    async fn stop_unwanted(&self, rows: &[JobRow]) -> Result<(), StoreError> {
        let mut unwanted = Vec::new();
        self.slots.prune_tasks();
        for row in rows {
            if row.job.next_command().is_none()
                && let Some(handle) = self.slots.begin_stop(row.id)
            {
                self.slots.free(row.id);
                unwanted.push((row.id, handle));
            }
        }
        for (id, handle) in unwanted {
            let _ = handle.await;
            self.executor.stop(id).await;
            self.slots.end_stop(id);
            self.store.append_factory_log(
                id,
                "Stopped the running step because the job no longer needs it.",
            )?;
        }
        Ok(())
    }

    fn workers_free(&self, settings: &Settings) -> bool {
        let busy = self.slots.workers_running();
        busy < settings.max_workers
    }

    /// The project a job runs against, or `None` when the job cannot go on
    /// this pass. An unknown project fails the job. A read error is logged and
    /// left for the next pass, so a transient fault does not fail the job.
    fn project_for(&self, row: &JobRow, job: &Job) -> Result<Option<Project>, StoreError> {
        match self.projects.lookup(&row.project) {
            Ok(project @ Some(_)) => Ok(project),
            Ok(None) => {
                self.fail_unknown_project(row, job)?;
                Ok(None)
            }
            Err(err) => {
                tracing::error!("job {}: cannot read project {}: {err}", row.id, row.project);
                Ok(None)
            }
        }
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

    /// The MCP config for an agent run, with the servers skipped because a
    /// stored header value would not decrypt. The config is `None` when no
    /// server is active for this project and stage. When one is, it issues a
    /// per-run proxy token scoped to the job and those servers;
    /// [`Scheduler::agent`] revokes it when the worker exits. The scheduler
    /// reads the registry and the project's settings on every run, so a settings
    /// change reaches a job in flight at its next run.
    /// Builds the run for the agent stage `expect` of job `row`, with the
    /// servers it left out for an undecryptable secret. The model, the effort
    /// and the agent all read from the project's per-stage settings on the same
    /// mapping, and the session's recorded agent wins over the stage's for a
    /// resumed session, see [`stage_agent`] and [`Store::session_uuid`].
    fn agent_request(
        &self,
        row: &JobRow,
        job: &Job,
        project: &Project,
        session: Session,
        task: &factory_core::Task,
        expect: Expect,
    ) -> Result<(AgentRequest, Vec<SkippedServer>), StoreError> {
        let models = self.store.project_models(&row.project, &project.models)?;
        let model = match expect {
            Expect::Clarify => models.clarify,
            Expect::Spec => models.spec,
            Expect::Plan => models.plan,
            Expect::Commits | Expect::FixCi => models.implement,
            Expect::Review => models.review,
        };
        let efforts = self.store.project_efforts(&row.project)?;
        let effort = match expect {
            Expect::Clarify => efforts.clarify,
            Expect::Spec => efforts.spec,
            Expect::Plan => efforts.plan,
            Expect::Commits | Expect::FixCi => efforts.implement,
            Expect::Review => efforts.review,
        };
        let agents = self.store.project_agents(&row.project)?;
        let agent = stage_agent(&agents, expect);
        let (session_id, agent) = self.store.session_uuid(row.id, session, agent)?;
        let agent_session = self.store.agent_session(row.id, session)?;
        let selection = self.mcp_config(row.id, &row.project, expect)?;
        let mcp_servers = selection
            .config
            .as_ref()
            .map(|config| config.servers.as_slice())
            .unwrap_or_default();
        let request = AgentRequest {
            job_id: row.id,
            repo: project.repo.clone(),
            base: project.base.clone(),
            branch: match session {
                Session::Planning | Session::Spec | Session::Plan => None,
                Session::Pr(pr) | Session::Review(pr) => Some(branch_name(row.id, pr)),
            },
            session_id,
            agent_session,
            agent,
            expect,
            prompt: job_prompt(job, task, expect, agent, mcp_servers),
            model,
            effort: Some(effort),
            mcp: selection.config,
        };
        Ok((request, selection.skipped))
    }

    fn mcp_config(
        &self,
        job_id: i64,
        project: &str,
        expect: Expect,
    ) -> Result<McpSelection, StoreError> {
        let ActiveServers { active, skipped } = self.active_mcp_servers(project, expect)?;
        let config = (!active.is_empty()).then(|| {
            let token = self.mcp.tokens.issue(job_id, project, &active);
            McpConfig {
                proxy_url: self.mcp.proxy_url.clone(),
                token,
                servers: active,
            }
        });
        Ok(McpSelection { config, skipped })
    }

    /// The servers `project` turns on for the run's `expect` stage, in name
    /// order, and the servers left out because a stored header value cannot be
    /// decrypted, each with a reason for the run's warning. A disabled server
    /// and one that does not pick this stage are left out of both, so they cause
    /// no warning.
    fn active_mcp_servers(
        &self,
        project: &str,
        expect: Expect,
    ) -> Result<ActiveServers, StoreError> {
        let mut active = Vec::new();
        let mut skipped = Vec::new();
        for server in self.store.mcp_servers()? {
            let settings = self.store.project_mcp(project, &server.name)?;
            let in_stage = match expect {
                Expect::Clarify => settings.stages.clarify,
                Expect::Spec => settings.stages.spec,
                Expect::Plan => settings.stages.plan,
                Expect::Commits | Expect::FixCi => settings.stages.implement,
                Expect::Review => settings.stages.review,
            };
            if !(settings.enabled && in_stage) {
                continue;
            }
            if self.mcp_secrets_usable(project, &server)? {
                active.push(server.name);
            } else {
                skipped.push((
                    server.name,
                    "a stored header value could not be decrypted".to_owned(),
                ));
            }
        }
        Ok(ActiveServers { active, skipped })
    }

    /// Whether every stored header value `project` set for `server` decrypts, so
    /// the proxy can forward the real credential. A header with no stored value
    /// is fine, so a keyless server such as context7 is always usable. A stored
    /// value that does not decrypt, or a missing master key with a value stored,
    /// skips the server, so a broken credential never lands in a worker's
    /// config. A transient database error propagates and the run waits.
    fn mcp_secrets_usable(&self, project: &str, server: &McpServer) -> Result<bool, StoreError> {
        for header in &server.headers {
            let at = SecretRef {
                project,
                server: &server.name,
                header,
            };
            match self.mcp.secrets.get(at) {
                // Present and decrypts, or not stored at all.
                Ok(_) => {}
                // A real database error: leave the run for the next pass.
                Err(SecretError::Store(err)) => return Err(err),
                // Unavailable, Decrypt or an unknown backend: unusable only when
                // a value is actually stored. An unset header stays keyless.
                Err(_) => {
                    if self.store.has_secret(at)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
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
            let (branch, base) = (request.branch.clone(), request.base.clone());
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
                Ok(Published::Empty) => {
                    scheduler.note(
                        job_id,
                        &format!("{branch} has no commits after {base}, so there is nothing to publish. The factory skips this pull request."),
                    );
                    scheduler.apply(job_id, Event::NothingToPublish);
                }
                Err(err) => scheduler.retry_later(&settings, job_id, &err),
            }
        });
        Ok(())
    }

    /// Spawns the watch of an open pull request, named by `command`, a
    /// [`Command::WatchPr`]. The request carries the project's auto-merge and
    /// LLM-review flags and whether the current head is already reviewed, so
    /// the executor can review before merge, merge, or wait. Reading the flags
    /// here means a settings change reaches a job in flight at its next watch.
    /// A `watch_only` command marks a watch for a stopped review: the executor
    /// then reports only a merge or close and takes no other action, so the
    /// flags above do not apply.
    fn watch_pr(
        self: &Arc<Self>,
        row: &JobRow,
        job: &Job,
        project: &Project,
        settings: &Settings,
        command: &Command,
    ) -> Result<(), StoreError> {
        let &Command::WatchPr {
            pr,
            number,
            watch_only,
        } = command
        else {
            return Ok(());
        };
        let request = WatchRequest {
            job_id: row.id,
            repo: project.repo.clone(),
            base: project.base.clone(),
            number,
            seen_until: self.store.pr_watermark(row.id, pr)?,
            auto_merge: self
                .store
                .project_auto_merge(&row.project, project.auto_merge)?,
            llm_review: self
                .store
                .project_llm_review(&row.project, project.llm_review)?,
            review_approved: job.review_state() == ReviewState::Approved,
            watch_only,
        };
        let scheduler = Arc::clone(self);
        let settings = settings.clone();
        self.spawn(row.id, Kind::Factory, async move {
            scheduler.watch(&settings, pr, request).await;
        });
        Ok(())
    }

    async fn agent(&self, settings: &Settings, request: AgentRequest, skipped: Vec<SkippedServer>) {
        let job_id = request.job_id;
        // The run's proxy token is revoked when this guard drops: at the end of
        // the run, or when a cancel aborts and drops this future.
        let _token = request.mcp.as_ref().map(|mcp| TokenGuard {
            tokens: Arc::clone(&self.mcp.tokens),
            token: mcp.token.clone(),
        });
        // Forget the servers warned about in this job's previous run before this
        // one reports any, and clear this run's set when the guard drops, so a
        // server is warned about at most once per run. Begun before the worker
        // starts, so a proxy warning during the run shares the same set.
        self.mcp.warnings.begin(job_id);
        let _warn = WarnGuard {
            warnings: Arc::clone(&self.mcp.warnings),
            job_id,
        };
        // A server skipped because its stored header value would not decrypt
        // never reaches the worker, so it gets its warning here, before the run.
        for (server, reason) in &skipped {
            self.mcp.warnings.record(job_id, server, reason);
        }
        // The session the run resumes: Codex's thread id once it has one, else
        // the factory UUID that names the session for Claude and a fresh Codex
        // run.
        let session_label = request
            .agent_session
            .clone()
            .unwrap_or_else(|| request.session_id.clone());
        self.note(
            job_id,
            &format!("Agent run started in session {session_label}."),
        );
        let sink_store = Arc::clone(&self.store);
        let sink: LogSink = Box::new(move |line| {
            if let Err(err) = sink_store.append_log(job_id, &line) {
                tracing::error!("job {job_id}: {err}");
            }
        });
        // The agent names its own session, such as Codex's thread id, mid-run.
        // Store it against the factory UUID the moment it arrives, so a run that
        // dies after that line resumes on it.
        let session_store = Arc::clone(&self.store);
        let session_uuid = request.session_id.clone();
        let cost_session = request.session_id.clone();
        let session: SessionSink = Box::new(move |id| {
            if let Err(err) = session_store.set_agent_session(&session_uuid, &id) {
                tracing::error!("job {job_id}: {err}");
            }
        });
        // The model the run passed, kept before the request moves into the run,
        // so a Codex run can look up its price by it afterwards.
        let model = request.model.clone();
        let agent = request.agent;
        let report = self
            .executor
            .run_agent(settings, request, sink, session)
            .await;
        // Each active server the agent read as not connected in the run's init
        // line gets its warning, deduped against a proxy warning for the same
        // server already recorded during the run.
        for unavailable in &report.mcp_unavailable {
            self.mcp
                .warnings
                .record(job_id, &unavailable.server, &unavailable.reason);
        }
        self.record_run_cost(job_id, &cost_session, agent, model.as_deref(), &report);
        self.apply(job_id, report.event);
    }

    /// Records the cost of a finished agent run and notes it on the job log. A
    /// run that reports a dollar figure, such as Claude's, reports the total of
    /// the session whose factory UUID is `session`, so the run records the
    /// increase over the total stored for it. A
    /// run that reports token counts instead, such as Codex's, is estimated from
    /// the price table for the model it ran: with a price row the estimate counts
    /// toward the job and project totals and is marked estimated, and with no row
    /// for the model, or no model at all, the run adds no cost and the log names
    /// the model. The token counts go to the log either way. A Copilot run adds
    /// no cost at all.
    fn record_run_cost(
        &self,
        job_id: i64,
        session: &str,
        agent: Agent,
        model: Option<&str>,
        report: &RunReport,
    ) {
        if agent == Agent::Copilot {
            // Copilot bills in credits the factory does not price, so the run
            // adds no cost row and the log says so once.
            self.note(
                job_id,
                "Copilot run finished. Copilot runs are not counted in the cost.",
            );
            return;
        }
        if let Some(total) = report.cost_usd {
            match self.store.add_session_cost(job_id, session, total) {
                Ok(usd) => {
                    let mut text =
                        format!("Agent run finished, {} at API prices", format_cost(usd));
                    if format_cost(usd) != format_cost(total) {
                        let _ = write!(text, ", {} for the session so far", format_cost(total));
                    }
                    text.push('.');
                    self.note(job_id, &text);
                }
                Err(err) => tracing::error!("job {job_id}: {err}"),
            }
            return;
        }
        let Some(usage) = report.token_usage else {
            self.note(job_id, "Agent run finished.");
            return;
        };
        let tokens = format!(
            "{} input tokens ({} cached), {} output tokens",
            usage.input_tokens, usage.cached_input_tokens, usage.output_tokens
        );
        let price = model.and_then(|model| match self.store.codex_price(model) {
            Ok(price) => price,
            Err(err) => {
                tracing::error!("job {job_id}: reading the Codex price failed: {err}");
                None
            }
        });
        match (model, price) {
            (Some(model), Some(price)) => {
                let usd = codex_cost(usage, &price);
                if let Err(err) = self.store.add_cost(job_id, usd, true) {
                    tracing::error!("job {job_id}: {err}");
                }
                self.note(
                    job_id,
                    &format!(
                        "Codex run finished, an estimated {} from the {model} price. {tokens}.",
                        format_cost(usd)
                    ),
                );
            }
            (Some(model), None) => self.note(
                job_id,
                &format!(
                    "Codex run finished. No price is set for the model {model}, \
                     so no cost was added. {tokens}."
                ),
            ),
            (None, _) => self.note(
                job_id,
                &format!(
                    "Codex run finished. The run passed no model, so no price is set \
                     and no cost was added. {tokens}."
                ),
            ),
        }
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
                if matches!(event, Event::StartReview { .. }) {
                    self.note(
                        job_id,
                        &format!("#{number} is green; reviewing it before the merge."),
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

    async fn rerun(&self, settings: &Settings, request: RerunRequest, reason: &str) {
        let job_id = request.job_id;
        let number = request.number;
        self.note(
            job_id,
            &format!("The agent asked to rerun CI on #{number}: {reason}"),
        );
        match self.executor.rerun(request).await {
            Ok(rerun) => {
                let text = match rerun {
                    Rerun::Started(ids) => {
                        format!("Reran {} failed CI run(s) on #{number}.", ids.len())
                    }
                    Rerun::NothingToRerun => {
                        format!("#{number} has no failed Actions run to rerun.")
                    }
                };
                self.note(job_id, &text);
                self.wait(settings, job_id);
                self.apply(job_id, Event::CiRerun);
            }
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
        let worker = kind == Kind::Worker;
        if worker {
            self.slots.take(job_id, unix_now());
        }
        self.slots.register_task(job_id, worker, |id| {
            tokio::spawn(async move {
                work.await;
                scheduler.slots.free(job_id);
                scheduler.slots.finish_task(job_id, id);
                scheduler.wake.notify_one();
            })
        });
    }
}
