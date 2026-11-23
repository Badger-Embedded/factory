use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use factory_core::{
    Agent, BranchSummary, Command, DEFAULT_AGENT, Event, ExistingGap, FixLink, FixOutcome,
    FixState, Gap, Job, MAX_FAILED_ROUNDS, PackagingContext, PrDescription, RetryLimits,
    ReviewState, Session, Stage,
};
use factory_runner::{
    Account, Expect, ForgeKind, HeadCommit, LogLine, McpConfig, Observation, RenovatePr, Rerun,
    RunReport, job_prompt,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::accounts::{Accounts, Resolved};
use crate::cost::{codex_cost, format_cost};
use crate::mcp::McpServer;
use crate::projects::{Project, Projects, StageAgents};
use crate::proxy::RunTokens;
use crate::secrets::{SecretError, SecretRef, SecretStore};
use crate::settings::Settings;
use crate::store::{Claim, FACTORY_REQUESTER, JobRow, Store, StoreError};
use crate::warnings::McpWarnings;
use crate::workers::{WorkerSlots, unix_now};

/// How often the loop looks at the jobs when nothing wakes it.
const POLL: Duration = Duration::from_secs(5);

/// The least time between two lists of a project's Renovate pull requests.
/// The spec asks for a pass at least every 10 minutes, so this leaves room
/// for a slow pass.
pub const RENOVATE_DISCOVERY: Duration = Duration::from_mins(5);

/// The review text of the approval of a safe Renovate update.
pub const APPROVAL: &str = "Every check passed and the diff changes only dependency or packaging \
                        version files.";

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
        Expect::Review | Expect::FailureAnalysis | Expect::DiffCheck | Expect::PackagingGaps => {
            &agents.review
        }
    };
    Agent::from_id(id).unwrap_or(DEFAULT_AGENT)
}

fn stage_name(expect: Expect) -> &'static str {
    match expect {
        Expect::Clarify => "clarify",
        Expect::Spec => "spec",
        Expect::Plan => "plan",
        Expect::Commits | Expect::FixCi => "implement",
        Expect::Review | Expect::FailureAnalysis | Expect::DiffCheck | Expect::PackagingGaps => {
            "review"
        }
    }
}

/// One agent run for the executor to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub job_id: i64,
    pub forge: ForgeKind,
    /// The account the operation runs as. Its token never reaches a worker.
    pub account: Account,
    /// The repository as `owner/name`.
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
    pub forge: ForgeKind,
    /// The account the operation runs as. Its token never reaches a worker.
    pub account: Account,
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
    pub forge: ForgeKind,
    /// The account the operation runs as. Its token never reaches a worker.
    pub account: Account,
    /// The logins whose comments are not review feedback.
    pub ignore: Vec<String>,
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

/// How asking Renovate to rebase a pull request went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebase {
    Requested,
    /// The description has no rebase checkbox.
    NoCheckbox,
    /// The head is no longer the one the fix job merged against.
    HeadMoved,
}

/// How a finished fix job ended, or `None` while it runs or can still be
/// retried. A merge returns `Merged` with an empty head, which the caller
/// fills in from the forge.
fn fix_outcome(fix: &Job) -> Option<FixOutcome> {
    match fix.stage() {
        Stage::Merged => {
            let numbered = fix.plan().iter().filter(|pr| pr.number.is_some()).count();
            Some(if numbered == 0 {
                FixOutcome::Closed
            } else {
                FixOutcome::Merged {
                    head: String::new(),
                }
            })
        }
        Stage::Cancelled => Some(FixOutcome::Cancelled),
        Stage::Failed { reason, .. } if !fix.can_retry() && reason == FIX_PR_CLOSED => {
            Some(FixOutcome::Closed)
        }
        Stage::Failed { .. } if fix.is_dismissed() => Some(FixOutcome::Dismissed),
        _ => None,
    }
}

/// The reason core gives a job whose pull request closed without merging.
const FIX_PR_CLOSED: &str = "the pull request was closed without merging";

/// One look at a Renovate pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenovateWatchRequest {
    pub job_id: i64,
    pub forge: ForgeKind,
    /// The account the operation runs as. Its token never reaches a worker.
    pub account: Account,
    /// The logins whose comments are not review feedback.
    pub ignore: Vec<String>,
    pub repo: String,
    pub number: u64,
    /// The head the job has a diff verdict for. The executor fetches the diff
    /// only for another head.
    pub verdict_head: Option<String>,
    /// The head the job last saw, which a `watch_only` look compares with.
    pub known_head: Option<String>,
    /// The job waits for the user, so the executor reports only a merge, a
    /// close, or a new head commit.
    pub watch_only: bool,
    /// The retry limits saved now, read on every watch.
    pub limits: RetryLimits,
    /// The project's auto-merge flag now.
    pub auto_merge: bool,
    /// The manifest files of a packaging project, or `None` for another
    /// project. They tell the executor whether a merge starts the packaging
    /// analysis.
    pub manifest_files: Option<Vec<String>>,
}

/// What the packaging analysis of a merged Renovate pull request reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagingRequest {
    pub forge: ForgeKind,
    /// The account the operation runs as. Its token never reaches a worker.
    pub account: Account,
    /// The maintained repository as `owner/name`.
    pub repo: String,
    /// The upstream repository as `owner/name`.
    pub upstream: String,
    /// The base branch, where the manifest files are read after the merge.
    pub base: String,
    pub number: u64,
    pub manifest_files: Vec<String>,
    pub build_config_files: Vec<String>,
    /// The head commit the diff check's refs belong to. The executor uses the
    /// refs only for a merge of this head.
    pub verdict_head: Option<String>,
    /// The upstream refs the diff check found.
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerunRequest {
    pub job_id: i64,
    pub forge: ForgeKind,
    /// The account the operation runs as. Its token never reaches a worker.
    pub account: Account,
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

    /// Runs the project's checks in a worker. An `Err` here counts as one
    /// checks error. After [`MAX_FAILED_ROUNDS`] in a row the job fails
    /// instead of trying again.
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

    /// Lists the open pull requests Renovate made against `base`.
    fn renovate_prs(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        base: &str,
    ) -> impl Future<Output = Result<Vec<RenovatePr>, String>> + Send;

    /// Looks at a Renovate pull request and returns the event to report, or
    /// `None` when there is nothing to report.
    fn watch_renovate(
        &self,
        request: RenovateWatchRequest,
    ) -> impl Future<Output = Result<Option<Event>, String>> + Send;

    /// Posts `body` as a comment on the pull request.
    fn comment(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        body: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Approves the pull request with a plain review text and squash-merges
    /// `head`, and only that head.
    fn merge_renovate(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Reads the head commit of the pull request and when it became the head.
    fn renovate_head(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> impl Future<Output = Result<HeadCommit, String>> + Send;

    /// When the pull request merged, or `None` while it is not merged.
    fn merged_at(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> impl Future<Output = Result<Option<String>, String>> + Send;

    /// Asks Renovate to rebase the pull request, unless its head is no longer
    /// `head`. The check and the description the write builds on come from
    /// one read.
    fn request_rebase(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        head: &str,
    ) -> impl Future<Output = Result<Rebase, String>> + Send;

    /// The event for a merged Renovate pull request. `manifest_files` is
    /// `Some` for a packaging project, and the event then says whether the
    /// pull request changed one of them.
    fn renovate_merged(
        &self,
        forge: ForgeKind,
        account: &Account,
        repo: &str,
        number: u64,
        manifest_files: Option<&[String]>,
    ) -> impl Future<Output = Result<Event, String>> + Send;

    /// Reads the pull request, the manifest files on the base branch and the
    /// upstream patches for the packaging analysis. The result has no
    /// packaging type and no existing gap jobs; the scheduler adds them.
    fn gather_packaging(
        &self,
        request: PackagingRequest,
    ) -> impl Future<Output = Result<PackagingContext, String>> + Send;

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
    accounts: Accounts,
    wake: Arc<Notify>,
    data_dir: PathBuf,
    /// Jobs whose factory step waits until the given time: the next look at
    /// a pull request, or a retry after a failed step.
    not_before: Mutex<HashMap<i64, Instant>>,
    slots: Arc<WorkerSlots>,
    mcp: McpRuntime,
    /// When each project's Renovate pull requests were last listed.
    discovered: Mutex<HashMap<String, Instant>>,
}

impl<E: Executor> Scheduler<E> {
    pub fn new(
        store: Arc<Store>,
        projects: Arc<Projects>,
        executor: Arc<E>,
        wake: Arc<Notify>,
        data_dir: PathBuf,
        mcp: McpRuntime,
        accounts: Accounts,
    ) -> Arc<Self> {
        Arc::new(Scheduler {
            store,
            projects,
            executor,
            accounts,
            wake,
            data_dir,
            not_before: Mutex::new(HashMap::new()),
            slots: Arc::new(WorkerSlots::new()),
            mcp,
            discovered: Mutex::new(HashMap::new()),
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
        self.discover_renovate().await?;
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
                job = match self.store.start(row.id) {
                    Ok(Some(started)) => started.job,
                    Ok(None) | Err(StoreError::Rejected(_) | StoreError::NotFound(_)) => continue,
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
                    let resolved = self.resolve(&row, &project)?;
                    let (request, skipped) = self
                        .agent_request(&row, &job, &project, &resolved, session, &task, expect)?;
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
                    let resolved = self.resolve(&row, &project)?;
                    self.publish(row.id, pr, &job, &project, &resolved, &settings)?;
                }
                watch @ Command::WatchPr { .. } if self.due(row.id) => {
                    let resolved = self.resolve(&row, &project)?;
                    self.watch_pr(&row, &job, &project, &resolved, &settings, &watch)?;
                }
                Command::RerunCi { number, reason, .. } if self.due(row.id) => {
                    let resolved = self.resolve(&row, &project)?;
                    let request = RerunRequest {
                        job_id: row.id,
                        forge: resolved.forge,
                        account: resolved.account,
                        repo: project.repo.clone(),
                        number,
                    };
                    let scheduler = Arc::clone(self);
                    let settings = settings.clone();
                    self.spawn(row.id, Kind::Factory, async move {
                        scheduler.rerun(&settings, request, &reason, true).await;
                    });
                }
                renovate @ (Command::RerunRenovate { .. }
                | Command::WatchRenovate { .. }
                | Command::PostComment { .. }
                | Command::MergeRenovate { .. }
                | Command::RequestRebase { .. }
                | Command::GatherPackaging { .. })
                    if self.due(row.id) =>
                {
                    self.renovate_command(&row, &job, &project, &settings, renovate)?;
                }
                Command::ProposeGaps { gaps, range } => {
                    self.propose_gaps(row.id, &gaps, range.as_ref());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Starts the factory turn of a Renovate job.
    fn renovate_command(
        self: &Arc<Self>,
        row: &JobRow,
        job: &Job,
        project: &Project,
        settings: &Settings,
        command: Command,
    ) -> Result<(), StoreError> {
        let id = row.id;
        let resolved = self.resolve(row, project)?;
        match command {
            Command::RerunRenovate { number, reason } => {
                let request = RerunRequest {
                    job_id: id,
                    forge: resolved.forge,
                    account: resolved.account,
                    repo: project.repo.clone(),
                    number,
                };
                let scheduler = Arc::clone(self);
                let settings = settings.clone();
                self.spawn(id, Kind::Factory, async move {
                    scheduler.rerun(&settings, request, &reason, false).await;
                });
            }
            Command::WatchRenovate {
                number,
                verdict_head,
                watch_only,
            } => {
                let request = RenovateWatchRequest {
                    job_id: id,
                    forge: resolved.forge,
                    account: resolved.account,
                    ignore: resolved.ignore,
                    repo: project.repo.clone(),
                    number,
                    verdict_head,
                    known_head: job.renovate_pr().and_then(|pr| pr.head.clone()),
                    watch_only,
                    limits: RetryLimits {
                        auto: settings.renovate_auto_reruns,
                        total: settings.renovate_total_reruns,
                    },
                    auto_merge: self
                        .store
                        .project_auto_merge(&row.project, project.auto_merge)?,
                    manifest_files: self.manifest_files(&row.project)?,
                };
                let scheduler = Arc::clone(self);
                let settings = settings.clone();
                self.spawn(id, Kind::Factory, async move {
                    scheduler.watch_renovate(&settings, request).await;
                });
            }
            Command::PostComment { number, body } => {
                let repo = project.repo.clone();
                let scheduler = Arc::clone(self);
                let settings = settings.clone();
                self.spawn(id, Kind::Factory, async move {
                    scheduler
                        .post_comment(&settings, id, &resolved, &repo, number, &body)
                        .await;
                });
            }
            Command::MergeRenovate { number, head } => {
                let repo = project.repo.clone();
                let manifest_files = self.manifest_files(&row.project)?;
                let scheduler = Arc::clone(self);
                let settings = settings.clone();
                self.spawn(id, Kind::Factory, async move {
                    scheduler
                        .merge_renovate(
                            &settings,
                            id,
                            &resolved,
                            &repo,
                            number,
                            &head,
                            manifest_files,
                        )
                        .await;
                });
            }
            Command::RequestRebase { number, head } => {
                let repo = project.repo.clone();
                let scheduler = Arc::clone(self);
                self.spawn(id, Kind::Factory, async move {
                    scheduler
                        .request_rebase(id, &resolved, &repo, number, &head)
                        .await;
                });
            }
            Command::GatherPackaging { number } => {
                self.start_gathering(row, job, project, &resolved, settings, number)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn start_gathering(
        self: &Arc<Self>,
        row: &JobRow,
        job: &Job,
        project: &Project,
        resolved: &Resolved,
        settings: &Settings,
        number: u64,
    ) -> Result<(), StoreError> {
        let id = row.id;
        let Some(packaging) = self.store.project_maintenance(&row.project)?.packaging else {
            self.apply(
                id,
                Event::RenovateMerged {
                    packaging: false,
                    manifest_changed: false,
                },
            );
            return Ok(());
        };
        let verdict = job.renovate_pr().and_then(|pr| pr.verdict.as_ref());
        let request = PackagingRequest {
            forge: resolved.forge,
            account: resolved.account.clone(),
            repo: project.repo.clone(),
            upstream: packaging.upstream_repo,
            base: project.base.clone(),
            number,
            manifest_files: packaging.manifest_files,
            build_config_files: packaging.build_config_files,
            verdict_head: job.renovate_pr().and_then(|pr| pr.head.clone()),
            from: verdict.and_then(|verdict| verdict.from.clone()),
            to: verdict.and_then(|verdict| verdict.to.clone()),
        };
        let kind = packaging.kind.as_str().to_owned();
        let existing = self
            .store
            .gap_jobs(&row.project)?
            .into_iter()
            .map(|gap| ExistingGap {
                request: gap.request,
                dismissed: gap.dismissed,
            })
            .collect();
        let scheduler = Arc::clone(self);
        let settings = settings.clone();
        self.spawn(id, Kind::Factory, async move {
            scheduler
                .gather_packaging(&settings, id, request, kind, existing)
                .await;
        });
        Ok(())
    }

    fn resolve(&self, row: &JobRow, project: &Project) -> Result<Resolved, StoreError> {
        self.accounts
            .resolve(&row.project, project, &row.requested_by)
    }

    fn manifest_files(&self, project: &str) -> Result<Option<Vec<String>>, StoreError> {
        Ok(self
            .store
            .project_maintenance(project)?
            .packaging
            .map(|packaging| packaging.manifest_files))
    }

    async fn gather_packaging(
        &self,
        settings: &Settings,
        job_id: i64,
        request: PackagingRequest,
        kind: String,
        existing: Vec<ExistingGap>,
    ) {
        self.wait(settings, job_id);
        match self.executor.gather_packaging(request).await {
            Ok(mut context) => {
                context.kind = kind;
                context.existing = existing;
                self.apply(job_id, Event::PackagingGathered(Box::new(context)));
            }
            Err(err) => self.retry_later(settings, job_id, &err),
        }
    }

    /// Creates a job for each gap and records them on the Renovate job in one
    /// store transaction, so a retry after a crash never creates them twice.
    fn propose_gaps(&self, job_id: i64, gaps: &[Gap], range: Option<&(String, String)>) {
        match self.store.propose_gaps(job_id, gaps, range) {
            Ok(row) => {
                let found = row
                    .job
                    .renovate_pr()
                    .and_then(|pr| pr.gaps.as_ref())
                    .map_or(0, Vec::len);
                self.note(
                    job_id,
                    &format!("The packaging analysis found {found} gap(s)."),
                );
            }
            Err(err) => self.report(job_id, Err(err)),
        }
    }

    /// Lists the Renovate pull requests of each project with the switch on, at
    /// most once per [`RENOVATE_DISCOVERY`], and creates a job for each one
    /// that has none. A project with the switch off gets no call. A listing
    /// that fails is logged and tried again on the next pass.
    async fn discover_renovate(&self) -> Result<(), StoreError> {
        for (name, project) in self.projects.list()? {
            if !self.store.project_maintenance(&name)?.renovate {
                continue;
            }
            let due = self
                .discovered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&name)
                .is_none_or(|last| last.elapsed() >= RENOVATE_DISCOVERY);
            if !due {
                continue;
            }
            let resolved = match self.accounts.resolve(&name, &project, FACTORY_REQUESTER) {
                Ok(resolved) => resolved,
                Err(err) => {
                    tracing::error!("skipping the Renovate discovery of {name}: {err}");
                    continue;
                }
            };
            let listed = self
                .executor
                .renovate_prs(
                    resolved.forge,
                    &resolved.account,
                    &project.repo,
                    &project.base,
                )
                .await;
            let prs = match listed {
                Ok(prs) => prs,
                Err(err) => {
                    tracing::error!("listing the Renovate pull requests of {name}: {err}");
                    continue;
                }
            };
            self.discovered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(name.clone(), Instant::now());
            for pr in prs {
                self.store
                    .create_renovate_job(&name, pr.number, &pr.title, &pr.url)?;
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
            Ok(Some(project)) => {
                if let Some(problem) = project.forge.problem() {
                    tracing::error!("job {}: project {}: {problem}", row.id, row.project);
                    self.fail_unsupported_forge(row, job, &problem)?;
                    return Ok(None);
                }
                Ok(Some(project))
            }
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

    fn fail_unsupported_forge(
        &self,
        row: &JobRow,
        job: &Job,
        reason: &str,
    ) -> Result<(), StoreError> {
        let reason = reason.to_owned();
        let event = if matches!(job.next_command(), Some(Command::RunAgent { .. })) {
            Event::AgentFailed { reason }
        } else {
            Event::FactoryFailed { reason }
        };
        self.store.apply(row.id, event)?;
        Ok(())
    }

    /// The MCP config for an agent run, with the servers skipped because a
    /// stored header value would not decrypt. The config is `None` when no
    /// server is active for this project and stage. When one is, it issues a
    /// per-run proxy token scoped to the job and those servers;
    /// [`Scheduler::agent`] revokes it when the worker exits. The scheduler
    /// reads the registry and the project's settings on every run, so a settings
    /// change reaches a job in flight at its next run.
    #[allow(
        clippy::too_many_arguments,
        reason = "each argument is a separate fact of the operation that starts"
    )]
    fn agent_request(
        &self,
        row: &JobRow,
        job: &Job,
        project: &Project,
        resolved: &Resolved,
        session: Session,
        task: &factory_core::Task,
        expect: Expect,
    ) -> Result<(AgentRequest, Vec<SkippedServer>), StoreError> {
        let agents = self.store.project_agents(&row.project)?;
        let stage = stage_agent(&agents, expect);
        let (session_id, agent) = self.store.session_uuid(row.id, session, stage)?;
        let (model, effort) = if agent == stage {
            let models = self.store.project_models(&row.project, &project.models)?;
            let model = match expect {
                Expect::Clarify => models.clarify,
                Expect::Spec => models.spec,
                Expect::Plan => models.plan,
                Expect::Commits | Expect::FixCi => models.implement,
                Expect::Review
                | Expect::FailureAnalysis
                | Expect::DiffCheck
                | Expect::PackagingGaps => models.review,
            };
            let efforts = self.store.project_efforts(&row.project)?;
            let effort = match expect {
                Expect::Clarify => efforts.clarify,
                Expect::Spec => efforts.spec,
                Expect::Plan => efforts.plan,
                Expect::Commits | Expect::FixCi => efforts.implement,
                Expect::Review
                | Expect::FailureAnalysis
                | Expect::DiffCheck
                | Expect::PackagingGaps => efforts.review,
            };
            self.store
                .record_session_settings(row.id, session, model.as_deref(), &effort)?;
            (model, effort)
        } else {
            let last = self.store.session_settings(row.id, session)?;
            let effort = last
                .as_ref()
                .and_then(|last| last.effort.clone())
                .unwrap_or_else(|| agent.default_effort().to_owned());
            self.store.append_factory_log(
                row.id,
                &format!(
                    "This session started on {from}, so it stays on {from} with its earlier model. \
                     The {stage_name} stage is now set to {to}, which new sessions use.",
                    from = agent.display_name(),
                    to = stage.display_name(),
                    stage_name = stage_name(expect),
                ),
            )?;
            (last.and_then(|last| last.model), effort)
        };
        let agent_session = self.store.agent_session(row.id, session)?;
        let selection = self.mcp_config(row.id, &row.project, expect)?;
        let mcp_servers = selection
            .config
            .as_ref()
            .map(|config| config.servers.as_slice())
            .unwrap_or_default();
        let request = AgentRequest {
            job_id: row.id,
            forge: resolved.forge,
            account: resolved.account.clone(),
            repo: project.repo.clone(),
            base: project.base.clone(),
            branch: match session {
                Session::Planning | Session::Spec | Session::Plan | Session::Renovate(_) => None,
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
                Expect::Review
                | Expect::FailureAnalysis
                | Expect::DiffCheck
                | Expect::PackagingGaps => settings.stages.review,
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
                Err(err) => {
                    let failed = match scheduler
                        .store
                        .apply(job_id, Event::ChecksError { error: err.clone() })
                    {
                        Ok(row) => matches!(row.job.stage(), Stage::Failed { .. }),
                        Err(error) => {
                            scheduler.report(job_id, Err(error));
                            false
                        }
                    };
                    if failed {
                        scheduler.note(
                            job_id,
                            &format!(
                                "{err}. The checks could not run {MAX_FAILED_ROUNDS} times in a \
                                 row, so the job failed."
                            ),
                        );
                        scheduler.wake.notify_one();
                    } else {
                        scheduler.retry_later(&settings, job_id, &err);
                    }
                }
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
        resolved: &Resolved,
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
            forge: resolved.forge,
            account: resolved.account.clone(),
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
        resolved: &Resolved,
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
            forge: resolved.forge,
            account: resolved.account.clone(),
            ignore: resolved.ignore.clone(),
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

    async fn watch_renovate(&self, settings: &Settings, request: RenovateWatchRequest) {
        let job_id = request.job_id;
        self.wait(settings, job_id);
        self.follow_fix(&request).await;
        match self.executor.watch_renovate(request).await {
            Ok(Some(event)) => self.apply(job_id, event),
            Ok(None) => {}
            Err(err) => self.retry_later(settings, job_id, &err),
        }
    }

    /// Reports how the fix job of Renovate job `job_id` ended, when it did.
    /// It reads the fix job on every watch, since a watch of an unchanged
    /// head reports nothing.
    async fn follow_fix(&self, request: &RenovateWatchRequest) {
        let job_id = request.job_id;
        let Ok(row) = self.store.get(job_id) else {
            return;
        };
        let Some(pr) = row.job.renovate_pr() else {
            return;
        };
        let Some(FixLink {
            job: fix_id,
            state: FixState::Running,
            ..
        }) = &pr.fix
        else {
            return;
        };
        let fix_id = *fix_id;
        // A reused id belongs to another job, so it counts as deleted.
        let mut outcome = match self.store.get(fix_id) {
            Ok(fix) if fix.job.fix_source() == Some((job_id, pr.number)) => {
                match fix_outcome(&fix.job) {
                    Some(outcome) => outcome,
                    None => return,
                }
            }
            Ok(_) | Err(StoreError::NotFound(_)) => FixOutcome::Deleted,
            Err(err) => {
                tracing::error!("reading fix job #{fix_id}: {err}");
                return;
            }
        };
        if matches!(outcome, FixOutcome::Merged { .. }) {
            match self.merge_head(request, fix_id).await {
                Ok(merged) => outcome = merged,
                Err(err) => {
                    tracing::warn!("reading the head of #{}: {err}", request.number);
                    return;
                }
            }
        }
        self.apply(
            job_id,
            Event::FixEnded {
                job: fix_id,
                outcome,
            },
        );
    }

    /// How fix job `fix_id` merged relative to Renovate's pull request head.
    /// `Merged` with the empty head means the head became the head after the
    /// merge, so no rebase is due. `MergedUnordered` carries the current head
    /// and means the evidence does not say. The evidence, in order:
    ///
    /// 1. The force push GitHub dated: it decides.
    /// 2. A commit made after the merge: it became the head after it.
    /// 3. The factory saw the head before the merge: it was the head by then.
    ///
    /// A plain push has no date on GitHub, and a commit date only bounds it
    /// from below, so a head with none of this evidence is unknown.
    async fn merge_head(
        &self,
        request: &RenovateWatchRequest,
        fix_id: i64,
    ) -> Result<FixOutcome, String> {
        let fix = self.store.get(fix_id).map_err(|err| err.to_string())?;
        let mut merged_at: Option<String> = None;
        for number in fix.job.plan().iter().filter_map(|pr| pr.number) {
            let at = self
                .executor
                .merged_at(request.forge, &request.account, &request.repo, number)
                .await?;
            merged_at = merged_at.max(at);
        }
        let merged_at = merged_at.ok_or("the forge shows no merged fix pull request")?;
        let head = self
            .executor
            .renovate_head(
                request.forge,
                &request.account,
                &request.repo,
                request.number,
            )
            .await?;
        if let Some(pushed_at) = &head.pushed_at {
            return Ok(FixOutcome::Merged {
                head: if *pushed_at > merged_at {
                    String::new()
                } else {
                    head.sha
                },
            });
        }
        if head.committed_at.as_ref().is_some_and(|at| *at > merged_at) {
            return Ok(FixOutcome::Merged {
                head: String::new(),
            });
        }
        let seen = self
            .store
            .head_first_seen(request.job_id, &head.sha)
            .map_err(|err| err.to_string())?;
        Ok(match seen {
            Some(seen) if seen <= merged_at => FixOutcome::Merged { head: head.sha },
            _ => FixOutcome::MergedUnordered { head: head.sha },
        })
    }

    async fn request_rebase(
        &self,
        job_id: i64,
        resolved: &Resolved,
        repo: &str,
        number: u64,
        head: &str,
    ) {
        if head.is_empty() {
            self.apply(job_id, Event::RebaseSkipped);
            return;
        }
        // The claim comes before the write, and the write needs a pending fix
        // and a stored claim. A claim left by a run that died before it stored
        // the result means the write may have happened, so the factory asks
        // the user instead of writing again.
        let fix = match self.store.get(job_id) {
            Ok(row) => row
                .job
                .renovate_pr()
                .and_then(|pr| pr.fix.as_ref().map(|link| link.job)),
            Err(err) => {
                tracing::error!("reading job #{job_id}: {err}");
                return;
            }
        };
        let Some(fix) = fix else {
            return;
        };
        match self.store.claim_rebase(job_id, fix, head) {
            Ok(Claim::Granted) => {}
            Ok(Claim::Stale) => return,
            Ok(Claim::Taken) => {
                let reason = "the factory restarted while asking Renovate to rebase".to_owned();
                self.note(
                    job_id,
                    &format!("Could not ask Renovate to rebase: {reason}"),
                );
                self.apply(job_id, Event::RebaseFailed { reason });
                return;
            }
            Err(err) => {
                tracing::error!("claiming the rebase of job #{job_id}: {err}");
                return;
            }
        }
        let event = match self
            .executor
            .request_rebase(resolved.forge, &resolved.account, repo, number, head)
            .await
        {
            Ok(Rebase::Requested) => {
                self.note(
                    job_id,
                    &format!("Asked Renovate to rebase #{number} after the fix job merged."),
                );
                Event::RebaseRequested
            }
            Ok(Rebase::HeadMoved) => Event::RebaseSkipped,
            Ok(Rebase::NoCheckbox) => Event::RebaseFailed {
                reason: format!("#{number} has no rebase checkbox"),
            },
            Err(reason) => {
                self.note(
                    job_id,
                    &format!("Could not ask Renovate to rebase: {reason}"),
                );
                Event::RebaseFailed { reason }
            }
        };
        self.apply(job_id, event);
    }

    async fn post_comment(
        &self,
        settings: &Settings,
        job_id: i64,
        resolved: &Resolved,
        repo: &str,
        number: u64,
        body: &str,
    ) {
        match self
            .executor
            .comment(resolved.forge, &resolved.account, repo, number, body)
            .await
        {
            Ok(()) => {
                self.note(job_id, &format!("Commented on #{number}: {body}"));
                self.apply(job_id, Event::Commented);
            }
            Err(err) => self.retry_later(settings, job_id, &err),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "each argument is a separate fact of the operation that starts"
    )]
    async fn merge_renovate(
        &self,
        settings: &Settings,
        job_id: i64,
        resolved: &Resolved,
        repo: &str,
        number: u64,
        head: &str,
        manifest_files: Option<Vec<String>>,
    ) {
        match self
            .executor
            .merge_renovate(resolved.forge, &resolved.account, repo, number, head)
            .await
        {
            Ok(()) => {
                self.note(
                    job_id,
                    &format!(
                        "Approved and merged #{number}: its checks passed and auto-merge is on."
                    ),
                );
                // The merge is done. When the factory cannot read the pull
                // request now, the job goes back to watching, which finds it
                // merged.
                match self
                    .executor
                    .renovate_merged(
                        resolved.forge,
                        &resolved.account,
                        repo,
                        number,
                        manifest_files.as_deref(),
                    )
                    .await
                {
                    Ok(event) => self.apply(job_id, event),
                    Err(err) => {
                        self.retry_later(settings, job_id, &err);
                        self.apply(job_id, Event::MergeFailed);
                    }
                }
            }
            Err(err) => {
                self.retry_later(settings, job_id, &err);
                self.apply(job_id, Event::MergeFailed);
            }
        }
    }

    async fn rerun(
        &self,
        settings: &Settings,
        request: RerunRequest,
        reason: &str,
        agent_asked: bool,
    ) {
        let renovate = !agent_asked;
        let job_id = request.job_id;
        let number = request.number;
        let text = if agent_asked {
            format!("The agent asked to rerun CI on #{number}: {reason}")
        } else {
            format!("Rerunning CI on #{number}: {reason}")
        };
        self.note(job_id, &text);
        match self.executor.rerun(request).await {
            Ok(Rerun::Started(ids)) => {
                self.note(
                    job_id,
                    &format!("Reran {} failed CI run(s) on #{number}.", ids.len()),
                );
                self.wait(settings, job_id);
                self.apply(job_id, Event::CiRerun);
            }
            Ok(Rerun::NothingToRerun) => {
                self.note(
                    job_id,
                    &format!("#{number} has no failed Actions run to rerun."),
                );
                self.wait(settings, job_id);
                let event = if renovate {
                    Event::NothingToRerun
                } else {
                    Event::CiRerun
                };
                self.apply(job_id, event);
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
