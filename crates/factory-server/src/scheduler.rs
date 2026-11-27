use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use factory_core::{
    Agent, BranchSummary, Cli, Command, DEFAULT_AGENT, Event, ExistingGap, FixLink, FixOutcome,
    FixState, Gap, Job, MAX_FAILED_ROUNDS, PackagingContext, PrDescription, PrWatch, RetryLimits,
    ReviewState, Session, Stage,
};
use factory_runner::{
    Expect, ForgeKind, HeadCommit, LogLine, McpConfig, McpProxy, McpServerEntry, NOT_PERMITTED,
    Observation, PromptTarget, ProviderKind, ProviderTarget, RenovatePr, Rerun, RunReport,
    job_prompt,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::accounts::{
    AccountSource, Accounts, ResolveError, Resolved, credential_failure, credential_reason,
};
use crate::cost::{codex_cost, format_cost};
use crate::mcp::{McpServer, McpTransport};
use crate::openrouter::{OPENROUTER_BASE_URL, OpenRouterCosts};
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
pub const APPROVAL: &str = "Every check passed, the diff only moves versions, and no breaking \
                        change in the new version affects this project.";

/// A finished job is deleted once it is this many days old, counted from
/// `created_at`.
pub const FINISHED_JOB_DAYS: u32 = 30;

/// The branch the factory pushes planned pull request `pr` of job `job_id`
/// from.
#[must_use]
pub fn branch_name(job_id: i64, pr: usize) -> String {
    format!("factory/job-{job_id}-pr-{}", pr + 1)
}

struct JobRefs {
    /// The branch of the job's checkout.
    branch: String,
    /// The branch the factory pushes to. It differs from `branch` for a job
    /// on a picked pull request, whose commits go to the pull request's own
    /// branch.
    remote_branch: String,
    base: String,
    /// The number of the picked pull request the checkout starts from.
    picked: Option<u64>,
    /// The job owns a pull request stacked on the picked one. It is rebased
    /// and pushed like any job's own pull request, not merged into.
    stacked: bool,
}

impl JobRefs {
    /// The pull request the checkout starts from, read on `forge`.
    fn existing(&self, forge: &ForgeKind) -> Option<ExistingPr> {
        self.picked.map(|number| ExistingPr {
            number,
            head_branch: self.remote_branch.clone(),
            head_ref: forge.pr_head_ref(number),
        })
    }
}

fn job_refs(job_id: i64, job: &Job, project: &Project, pr: usize) -> JobRefs {
    let branch = branch_name(job_id, pr);
    match job.picked_pr() {
        Some(picked) if job.stacked_on().is_some() => {
            // The reviewed pull request only seeds the branch. Once the stack
            // is open, its checkout and reads come from the stack itself.
            let opened = job.plan().get(pr).and_then(|planned| planned.number);
            JobRefs {
                remote_branch: branch.clone(),
                branch,
                base: job.stacked_base().unwrap_or(&picked.base_branch).to_owned(),
                picked: Some(opened.unwrap_or(picked.number)),
                stacked: true,
            }
        }
        Some(picked) => JobRefs {
            branch,
            remote_branch: picked.head_branch.clone(),
            base: picked.base_branch.clone(),
            picked: Some(picked.number),
            stacked: false,
        },
        None => JobRefs {
            remote_branch: branch.clone(),
            branch,
            base: project.base.clone(),
            picked: None,
            stacked: false,
        },
    }
}

/// The reviewed commit of a fork pull request that a stacked job's branch
/// must keep. `Err` when the job is a fork's stack and the review recorded
/// none, since the source ref a force-push can move is no substitute.
fn forked_head(job: &Job) -> Result<Option<String>, String> {
    if job.stacked_on().is_none_or(|picked| !picked.fork) {
        return Ok(None);
    }
    job.reviewed_head()
        .map(|head| Some(head.to_owned()))
        .ok_or_else(|| {
            "the quick review recorded no commit for the fork's pull request, so the factory \
             cannot check that the fix keeps it and publishes nothing"
                .to_owned()
        })
}

fn session_refs(job_id: i64, job: &Job, project: &Project, session: Session) -> JobRefs {
    let pr = match session {
        Session::Pr(pr) | Session::Review(pr) => pr,
        _ => 0,
    };
    job_refs(job_id, job, project, pr)
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
        Expect::Review
        | Expect::QuickReview
        | Expect::FailureAnalysis
        | Expect::DiffCheck
        | Expect::PackagingGaps => &agents.review,
    };
    Agent::from_id(id).unwrap_or(DEFAULT_AGENT)
}

fn stage_name(expect: Expect) -> &'static str {
    match expect {
        Expect::Clarify => "clarify",
        Expect::Spec => "spec",
        Expect::Plan => "plan",
        Expect::Commits | Expect::FixCi => "implement",
        Expect::Review
        | Expect::QuickReview
        | Expect::FailureAnalysis
        | Expect::DiffCheck
        | Expect::PackagingGaps => "review",
    }
}

/// One agent run for the executor to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub job_id: i64,
    pub forge: ForgeKind,
    /// Gives the account for each forge or git operation of the step when it
    /// starts. The token never reaches a worker.
    pub account: AccountSource,
    /// The repository as `owner/name`.
    pub repo: String,
    /// The branch pull requests target.
    pub base: String,
    /// The pull request branch, or `None` for a planning run, which reads
    /// the base branch.
    pub branch: Option<String>,
    /// The picked pull request `branch` starts from, or `None` for a branch
    /// the factory opens itself. The checkout then takes the pull request's
    /// head in and keeps the commits it holds.
    pub existing: Option<ExistingPr>,
    /// Start merging `origin/<base>` into the branch before the run, for a
    /// conflict with the picked pull request's base branch. The merge stays
    /// unfinished in the checkout when it conflicts, for the agent to resolve.
    pub merge_base: bool,
    /// The run repairs a checkout the checks left off the job branch. The
    /// executor keeps the checkout as it is, so the agent reads the request
    /// even when switching branches would be refused over its edits.
    pub keep_checkout: bool,
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
    /// The project's worker packages, read on every run.
    pub packages: Vec<String>,
    /// The local server and the CLI that drives it, set exactly when `agent` is
    /// [`Agent::Local`].
    pub provider: Option<ProviderRequest>,
}

/// A pull request that already exists, which a job adds commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingPr {
    pub number: u64,
    /// The pull request's branch on the project's repository.
    pub head_branch: String,
    /// The forge's ref of the pull request's head, which holds the commits to
    /// start from.
    pub head_ref: String,
}

/// What a Local run needs beyond an agent run's own fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    /// The server, the CLI and the key the run resolved. Its `Debug` hides the
    /// key.
    pub target: ProviderTarget,
    /// Whose key `target.key` is, for the job log.
    pub key_from: KeyOwner,
    /// The CLI the Local settings name now. It differs from `target.cli` when
    /// the run resumes a session that started on another CLI.
    pub setting_cli: Cli,
    /// The session the run belongs to. The executor records the CLI on it
    /// once the worker starts.
    pub session: Session,
}

/// Whose API key a Local run uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOwner {
    /// The stored key of the login that requested the job.
    Login(String),
    /// The factory-wide key.
    Factory,
    /// No key.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksRequest {
    pub job_id: i64,
    /// The branch the pull request targets.
    pub base: String,
    /// The picked pull request the branch starts from. The commits it already
    /// holds are not the factory's to check.
    pub existing: Option<u64>,
    /// The branch of the checkout, which a publish pushes.
    pub branch: String,
    /// The project's `checks` command.
    pub command: String,
    /// The project's worker packages, read on every run.
    pub packages: Vec<String>,
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
    /// Gives the account for each forge or git operation of the step when it
    /// starts. The token never reaches a worker.
    pub account: AccountSource,
    pub repo: String,
    pub base: String,
    /// The branch of the checkout.
    pub branch: String,
    /// The pull request that holds the branch's commits when the job works on
    /// a picked one. The publish then pushes fast-forward only to the pull
    /// request's own branch, and opens no pull request and edits no title or
    /// description, so `description` is not used.
    pub existing: Option<ExistingPr>,
    pub description: PrDescription,
    /// Whether the project runs checks before a push. A picked pull request
    /// job then never pushes a merge commit that the checks have not run on.
    pub checks: bool,
    /// The commit the fork pull request had when it was reviewed, which the
    /// job's own branch must keep. The push is then fast-forward only and refused when the
    /// branch lost that head, even on its first push.
    pub keep_history: Option<String>,
    /// Told the account of each push right after it succeeds, before the pull
    /// request is opened.
    pub pushed: PushLog,
}

/// Records which account a push used.
#[derive(Clone)]
pub struct PushLog(Arc<dyn Fn(&str) + Send + Sync>);

impl PushLog {
    pub fn new(log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        PushLog(Arc::new(log))
    }

    pub fn record(&self, pushed_as: &str) {
        (self.0)(pushed_as);
    }
}

impl std::fmt::Debug for PushLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PushLog")
    }
}

impl PartialEq for PushLog {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PushLog {}

/// What a publish found on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Published {
    /// This open pull request has the description.
    Open(u64),
    /// This pull request for the branch was merged before the push, so the
    /// factory pushed nothing.
    AlreadyMerged(u64),
    /// The branch has no commits after the base branch, so the factory
    /// pushed nothing and opened no pull request.
    Empty,
    /// The picked pull request was closed without merging before the push, so
    /// the factory pushed nothing.
    AlreadyClosed(u64),
    /// The push to a picked pull request was refused because someone pushed
    /// first. The factory merged their commits into the checkout, which is
    /// left unfinished when `conflicted`.
    HeadMoved { conflicted: bool },
    /// The job cannot go on, for this reason, and the factory pushed nothing.
    Stopped(String),
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact about the pull request or the watch mode"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRequest {
    pub job_id: i64,
    pub forge: ForgeKind,
    /// Gives the account for each forge or git operation of the step when it
    /// starts. The token never reaches a worker.
    pub account: AccountSource,
    pub repo: String,
    /// The branch the pull request targets.
    pub base: String,
    pub number: u64,
    /// The time of the newest comment the agent already has.
    pub seen_until: Option<String>,
    /// Merge the pull request when it is ready.
    pub auto_merge: bool,
    /// Whether the factory may merge the pull request at all. When false, a
    /// ready pull request is left to the user whatever `auto_merge` says.
    pub may_merge: bool,
    /// Run the LLM review before any merge. When it is on and the current head
    /// is not yet approved, a ready pull request starts a review instead of
    /// merging or waiting.
    pub llm_review: bool,
    /// The reviewer has approved the current head. A ready pull request then
    /// merges or waits for the user without another review pass.
    pub review_approved: bool,
    /// What the watch reports. Anything but [`PrWatch::Full`] means the job
    /// waits for the user: the watch does not merge or start a review, and
    /// it moves the comment watermark only when it delivers comments.
    pub watch: PrWatch,
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
    /// Leave the pull request for the user and tell the job, which asks the
    /// user to merge. For a job that may not merge.
    LeaveToUser,
}

/// Decides [`ReadyAction`] from the project's flags, the head's review state
/// and whether the job may merge. The review comes first: a reviewing project
/// reviews an unapproved head before it merges or waits. A job that may not
/// merge never gets [`ReadyAction::Merge`].
#[must_use]
#[allow(
    clippy::fn_params_excessive_bools,
    reason = "each flag is one independent project or job setting"
)]
pub fn ready_action(
    llm_review: bool,
    review_approved: bool,
    auto_merge: bool,
    may_merge: bool,
) -> ReadyAction {
    if llm_review && !review_approved {
        ReadyAction::Review
    } else if !may_merge {
        ReadyAction::LeaveToUser
    } else if auto_merge {
        ReadyAction::Merge
    } else {
        ReadyAction::Wait
    }
}

/// The event a watch reports while the job waits for the user. A merge or
/// close by someone else always counts: the job moves on when the user merges
/// the pull request, and fails when they close it. New comments count under
/// [`PrWatch::MergeCloseOrComments`], so a comment posted while the merge is
/// left to the user goes to the agent. Every other observation (green, CI
/// failed, conflict, nothing) returns `None`, so the watch never merges or
/// restarts the reviewer on its own. Pure, so the wiring is testable without
/// podman or a forge.
#[must_use]
pub fn waiting_event(watch: PrWatch, observation: &Observation) -> Option<Event> {
    match observation {
        Observation::Merged => Some(Event::Merged),
        Observation::Closed => Some(Event::ClosedUnmerged),
        Observation::Comments { text, .. } if watch == PrWatch::MergeCloseOrComments => {
            Some(Event::ReviewComments {
                comments: text.clone(),
            })
        }
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
    /// Gives the account for each forge or git operation of the step when it
    /// starts. The token never reaches a worker.
    pub account: AccountSource,
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
    /// Gives the account for each forge or git operation of the step when it
    /// starts. The token never reaches a worker.
    pub account: AccountSource,
    /// The maintained repository as `owner/name`.
    pub repo: String,
    /// The upstream repository, in the form its forge uses.
    pub upstream: String,
    /// The forge the upstream lives on.
    pub upstream_forge: ForgeKind,
    /// Gives the credential that reads the upstream: the upstream token, the
    /// project token for an upstream on the project's forge, or no token.
    pub upstream_account: AccountSource,
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
    /// Gives the account for each forge or git operation of the step when it
    /// starts. The token never reaches a worker.
    pub account: AccountSource,
    pub repo: String,
    pub number: u64,
    /// Whether the rerun is a Renovate triage step, which checks the account's
    /// access before it writes.
    pub renovate: bool,
}

/// A login whose pull request comments a watch left out, so they never reach
/// the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoredCommenter {
    /// The forge says the login has no write access to the repository.
    NoWrite(String),
    /// The forge would not tell the job's account what the login may do.
    Unknown(String),
}

/// What a look at a pull request found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchReport {
    /// `None` while CI runs and nobody commented.
    pub event: Option<Event>,
    /// The new comment watermark: when `event` delivers comments, or the seed
    /// of a merge-wait watch that found none, with no event.
    pub seen_until: Option<String>,
    /// The executor merged the pull request because it was ready and
    /// auto-merge is on.
    pub merged_by_factory: bool,
    /// The commenters whose new comments the watch left out, because only
    /// comments from logins with write access reach the agent.
    pub ignored: Vec<IgnoredCommenter>,
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

    /// Lists the open pull requests Renovate made against `base`. `author` is
    /// the login Renovate runs as on a forge with no fixed Renovate account.
    fn renovate_prs(
        &self,
        forge: &ForgeKind,
        account: &AccountSource,
        repo: &str,
        base: &str,
        author: Option<&str>,
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
        forge: &ForgeKind,
        account: &AccountSource,
        repo: &str,
        number: u64,
        body: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Whether the account `author` already posted `body` as a comment of the
    /// pull request. The lookup reads with the job's account.
    fn has_comment(
        &self,
        forge: &ForgeKind,
        account: &AccountSource,
        author: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> impl Future<Output = Result<bool, String>> + Send;

    /// Approves the pull request with a plain review text and squash-merges
    /// `head`, and only that head.
    fn merge_renovate(
        &self,
        forge: &ForgeKind,
        account: &AccountSource,
        repo: &str,
        number: u64,
        head: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Reads the head commit of the pull request and when it became the head.
    fn renovate_head(
        &self,
        forge: &ForgeKind,
        account: &AccountSource,
        repo: &str,
        number: u64,
    ) -> impl Future<Output = Result<HeadCommit, String>> + Send;

    /// When the pull request merged, or `None` while it is not merged.
    fn merged_at(
        &self,
        forge: &ForgeKind,
        account: &AccountSource,
        repo: &str,
        number: u64,
    ) -> impl Future<Output = Result<Option<String>, String>> + Send;

    /// Asks Renovate to rebase the pull request, unless its head is no longer
    /// `head`. The check and the description the write builds on come from
    /// one read.
    fn request_rebase(
        &self,
        forge: &ForgeKind,
        account: &AccountSource,
        repo: &str,
        number: u64,
        head: &str,
    ) -> impl Future<Output = Result<Rebase, String>> + Send;

    /// The event for a merged Renovate pull request. `manifest_files` is
    /// `Some` for a packaging project, and the event then says whether the
    /// pull request changed one of them.
    fn renovate_merged(
        &self,
        forge: &ForgeKind,
        account: &AccountSource,
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

/// One server active for a run, with what the run's config needs from the
/// registry and the project.
enum ActiveServer {
    Remote(String),
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
    },
}

impl ActiveServer {
    fn name(&self) -> &str {
        match self {
            Self::Remote(name) | Self::Stdio { name, .. } => name,
        }
    }
}

/// The servers active for a run and the servers skipped for an undecryptable
/// secret, as [`Scheduler::active_mcp_servers`] sorts them.
struct ActiveServers {
    active: Vec<ActiveServer>,
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

/// The server and tool of an agent's `mcp__<server>__<tool>` tool name when the
/// server is one of the run's stdio servers. Server names hold no underscore, so
/// the first `__` after the prefix ends the server.
fn stdio_tool_call<'a>(name: &'a str, stdio: &[String]) -> Option<(&'a str, &'a str)> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    stdio.iter().any(|s| s == server).then_some((server, tool))
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
    /// Reads what an `OpenRouter` run was charged, after the run.
    costs: Arc<OpenRouterCosts>,
    /// When each project's Renovate pull requests were last listed.
    discovered: Mutex<HashMap<String, Instant>>,
    /// The (job, login) pairs whose ignored comments the job's log already
    /// explains, so each is noted once while the factory runs.
    ignored_noted: Mutex<HashSet<(i64, String)>>,
}

fn quick_review_marker(session: &str) -> String {
    format!("<!-- factory quick review {session} -->")
}

fn rerun_request(
    job_id: i64,
    resolved: &Resolved,
    project: &Project,
    number: u64,
    renovate: bool,
) -> RerunRequest {
    RerunRequest {
        job_id,
        forge: resolved.forge.clone(),
        account: resolved.source.clone(),
        repo: project.repo.clone(),
        number,
        renovate,
    }
}

fn openrouter_key_of(request: &AgentRequest) -> Option<String> {
    request
        .provider
        .as_ref()
        .filter(|provider| provider.target.kind == ProviderKind::OpenRouter)
        .and_then(|provider| provider.target.key.clone())
}

impl<E: Executor> Scheduler<E> {
    #[allow(
        clippy::too_many_arguments,
        reason = "the scheduler wires each long-lived dependency it shares with its tasks"
    )]
    pub fn new(
        store: Arc<Store>,
        projects: Arc<Projects>,
        executor: Arc<E>,
        wake: Arc<Notify>,
        data_dir: PathBuf,
        mcp: McpRuntime,
        accounts: Accounts,
        costs: OpenRouterCosts,
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
            costs: Arc::new(costs),
            discovered: Mutex::new(HashMap::new()),
            ignored_noted: Mutex::new(HashSet::new()),
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
                    let Some(resolved) = self.resolve(&row, &project, &settings, Some(expect))?
                    else {
                        continue;
                    };
                    let Some((request, skipped)) = self
                        .agent_request(&row, &job, &project, &resolved, session, &task, expect)?
                    else {
                        continue;
                    };
                    let scheduler = Arc::clone(self);
                    let settings = settings.clone();
                    let job = job.clone();
                    self.spawn(row.id, Kind::Worker, async move {
                        scheduler.agent(&settings, request, skipped, job).await;
                    });
                }
                Command::RunChecks { pr } => {
                    self.checks(&row, pr, &job, &project, &settings, workers_free)?;
                }
                Command::Summarize { pr } if self.due(row.id) => {
                    self.summarize(row.id, pr, &job, &project, &settings);
                }
                Command::Publish { pr } if self.due(row.id) => {
                    let Some(resolved) = self.resolve(&row, &project, &settings, None)? else {
                        continue;
                    };
                    self.publish(row.id, pr, &job, &project, &resolved, &settings)?;
                }
                watch @ Command::WatchPr { .. } if self.due(row.id) => {
                    let Some(resolved) = self.resolve(&row, &project, &settings, None)? else {
                        continue;
                    };
                    self.watch_pr(&row, &job, &project, &resolved, &settings, &watch)?;
                }
                Command::RerunCi { number, reason, .. } if self.due(row.id) => {
                    let Some(resolved) = self.resolve(&row, &project, &settings, None)? else {
                        continue;
                    };
                    let request = rerun_request(row.id, &resolved, &project, number, false);
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

    /// The account a Renovate command runs as. A quick review's comment is
    /// optional, so an account that cannot be read ends it like a forge error
    /// and does not retry.
    fn resolve_for(
        &self,
        row: &JobRow,
        job: &Job,
        project: &Project,
        settings: &Settings,
        command: &Command,
    ) -> Result<Option<Resolved>, StoreError> {
        if job.picked_pr().is_none() || !matches!(command, Command::PostComment { .. }) {
            return self.resolve(row, project, settings, None);
        }
        match self
            .accounts
            .resolve(&row.project, project, &row.requested_by)
        {
            Ok(resolved) => Ok(Some(resolved)),
            Err(ResolveError::Store(err)) => Err(err),
            Err(ResolveError::Account(reason)) => {
                let error = credential_failure(&reason);
                self.note(row.id, &format!("Could not comment: {error}"));
                self.apply(row.id, Event::CommentFailed { error });
                Ok(None)
            }
        }
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
        let Some(resolved) = self.resolve_for(row, job, project, settings, &command)? else {
            return Ok(());
        };
        match command {
            Command::RerunRenovate { number, reason } => {
                let request = rerun_request(id, &resolved, project, number, true);
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
                if !self.upstream_known(row)? {
                    return Ok(());
                }
                let request = RenovateWatchRequest {
                    job_id: id,
                    forge: resolved.forge.clone(),
                    account: resolved.source.clone(),
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
                let quick_review = job.picked_pr().is_some();
                let scheduler = Arc::clone(self);
                let settings = settings.clone();
                self.spawn(id, Kind::Factory, async move {
                    scheduler
                        .post_comment(&settings, id, &resolved, &repo, number, &body, quick_review)
                        .await;
                });
            }
            Command::MergeRenovate { number, head } => {
                if !self.upstream_known(row)? {
                    return Ok(());
                }
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
                if !self.upstream_known(row)? {
                    return Ok(());
                }
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
            forge: resolved.forge.clone(),
            account: resolved.source.clone(),
            repo: project.repo.clone(),
            upstream: packaging.upstream_repo,
            upstream_forge: packaging.upstream_forge.clone(),
            upstream_account: self.accounts.upstream_source(
                &row.project,
                project,
                &packaging.upstream_forge,
            ),
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

    /// The account the next operation of a job runs as, resolved now so a
    /// token or switch changed since the last operation applies. When the job
    /// has no account, an agent run fails and any other step tries again
    /// later, so Retry or the next pass works once a token is saved. The
    /// result is `None` then.
    fn resolve(
        &self,
        row: &JobRow,
        project: &Project,
        settings: &Settings,
        agent: Option<Expect>,
    ) -> Result<Option<Resolved>, StoreError> {
        match self
            .accounts
            .resolve(&row.project, project, &row.requested_by)
        {
            Ok(resolved) => Ok(Some(resolved)),
            Err(ResolveError::Store(err)) => Err(err),
            Err(ResolveError::Account(reason)) => {
                if let Some(expect) = agent {
                    self.store
                        .apply(row.id, factory_runner::failed_event(expect, reason))?;
                } else {
                    self.retry_later(settings, row.id, &credential_failure(&reason));
                }
                Ok(None)
            }
        }
    }

    /// Whether the project's upstream forge is registered. When it is not, the
    /// job fails with that reason and the other jobs run on, as for a project
    /// on an unknown forge.
    fn upstream_known(&self, row: &JobRow) -> Result<bool, StoreError> {
        match self.store.project_maintenance(&row.project) {
            Ok(_) => Ok(true),
            Err(err @ StoreError::UnknownUpstreamForge(_)) => {
                self.store.apply(
                    row.id,
                    Event::FactoryFailed {
                        reason: err.to_string(),
                    },
                )?;
                Ok(false)
            }
            Err(err) => Err(err),
        }
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
            let maintenance = match self.store.project_maintenance(&name) {
                Ok(maintenance) => maintenance,
                Err(err @ StoreError::UnknownUpstreamForge(_)) => {
                    tracing::warn!("skipping the Renovate discovery of {name}: {err}");
                    continue;
                }
                Err(err) => return Err(err),
            };
            if !maintenance.renovate {
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
                Err(ResolveError::Account(reason)) => {
                    tracing::warn!("skipping the Renovate discovery of {name}: {reason}");
                    self.discovered
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(name.clone(), Instant::now());
                    continue;
                }
                Err(ResolveError::Store(err)) => {
                    tracing::error!("skipping the Renovate discovery of {name}: {err}");
                    continue;
                }
            };
            let needs_author = !resolved.forge.has_fixed_renovate_account();
            let author = needs_author
                .then_some(maintenance.renovate_author.as_deref())
                .flatten();
            if needs_author && author.is_none() {
                tracing::warn!(
                    "skipping the Renovate discovery of {name}: set the Renovate account in its Maintenance settings"
                );
                self.discovered
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.clone(), Instant::now());
                continue;
            }
            let listed = self
                .executor
                .renovate_prs(
                    &resolved.forge,
                    &resolved.source,
                    &project.repo,
                    &project.base,
                    author,
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
    #[allow(
        clippy::too_many_lines,
        reason = "one request built from the session's settings, refs and MCP config"
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
    ) -> Result<Option<(AgentRequest, Vec<SkippedServer>)>, StoreError> {
        let agents = self.store.project_agents(&row.project)?;
        let renovate = match expect {
            Expect::FailureAnalysis | Expect::DiffCheck | Expect::PackagingGaps => {
                self.store.project_renovate_agent(&row.project)?
            }
            _ => None,
        };
        let stage = match &renovate {
            Some(over) => Agent::from_id(&over.agent).unwrap_or(DEFAULT_AGENT),
            None => stage_agent(&agents, expect),
        };
        let (session_id, agent) = self.store.session_uuid(row.id, session, stage)?;
        let mut stage_note = None;
        let recorded = self.store.session_settings(row.id, session)?;
        let resumed = matches!(session, Session::Renovate(_))
            && recorded.as_ref().is_some_and(|last| last.effort.is_some());
        let (model, effort) = if agent == stage && !resumed {
            let (model, effort) = match renovate {
                Some(over) => (over.model, over.effort),
                None => self.stage_settings(row, project, expect)?,
            };
            self.store
                .record_session_settings(row.id, session, model.as_deref(), &effort)?;
            (model, effort)
        } else {
            let last = recorded;
            let effort = last
                .as_ref()
                .and_then(|last| last.effort.clone())
                .unwrap_or_else(|| agent.default_effort().to_owned());
            let model = last.and_then(|last| last.model);
            let changed = agent != stage
                || renovate
                    .as_ref()
                    .is_some_and(|over| over.model != model || over.effort != effort);
            if changed {
                let setting = if renovate.is_some() {
                    "Renovate agent setting".to_owned()
                } else {
                    format!("{} stage", stage_name(expect))
                };
                stage_note = Some(format!(
                    "This session started on {from}, so it stays on {from} with its earlier model. \
                     The {setting} is now set to {to}, which new sessions use.",
                    from = agent.display_name(),
                    to = stage.display_name(),
                ));
            }
            (model, effort)
        };
        let local = if matches!(agent, Agent::Local | Agent::OpenRouter) {
            let resolved = if agent == Agent::Local {
                self.local_request(row, session, model.as_deref())?
            } else {
                self.openrouter_request(row, session, model.as_deref())?
            };
            match resolved {
                Ok(local) => Some(local),
                Err(reason) => {
                    self.open_run_log(row.id, None, stage_note.as_deref())?;
                    self.store
                        .apply(row.id, factory_runner::failed_event(expect, reason))?;
                    return Ok(None);
                }
            }
        } else {
            None
        };
        self.open_run_log(row.id, local.as_ref(), stage_note.as_deref())?;
        let agent_session = self.store.agent_session(row.id, session)?;
        let selection = self.mcp_config(row.id, &row.project, expect)?;
        let mcp_servers: Vec<String> = selection
            .config
            .as_ref()
            .map(|config| config.names().into_iter().map(str::to_owned).collect())
            .unwrap_or_default();
        let refs = session_refs(row.id, job, project, session);
        let request = AgentRequest {
            job_id: row.id,
            forge: resolved.forge.clone(),
            account: resolved.source.clone(),
            repo: project.repo.clone(),
            base: refs.base.clone(),
            branch: matches!(session, Session::Pr(_) | Session::Review(_))
                .then(|| refs.branch.clone()),
            existing: matches!(session, Session::Pr(_) | Session::Review(_))
                .then(|| refs.existing(&resolved.forge))
                .flatten(),
            merge_base: refs.picked.is_some()
                && (!refs.stacked || job.stacked_on().is_some_and(|picked| picked.fork))
                && matches!(task, factory_core::Task::Rebase { .. }),
            keep_checkout: refs.picked.is_some()
                && (matches!(task, factory_core::Task::FixChecks { report } if report.starts_with(factory_core::CHECKOUT_REPAIR))
                    || matches!(task, factory_core::Task::Answer { .. })
                        && job.repairing_checkout()),
            session_id,
            agent_session,
            agent,
            expect,
            prompt: job_prompt(
                job,
                task,
                expect,
                local.as_ref().map_or(PromptTarget::Agent(agent), |local| {
                    PromptTarget::Provider(local.target.cli)
                }),
                &mcp_servers,
            ),
            model,
            effort: Some(effort),
            mcp: selection.config,
            packages: self.store.project_packages(&row.project)?,
            provider: local,
        };
        Ok(Some((request, selection.skipped)))
    }

    fn stage_settings(
        &self,
        row: &JobRow,
        project: &Project,
        expect: Expect,
    ) -> Result<(Option<String>, String), StoreError> {
        let models = self.store.project_models(&row.project, &project.models)?;
        let model = match expect {
            Expect::Clarify => models.clarify,
            Expect::Spec => models.spec,
            Expect::Plan => models.plan,
            Expect::Commits | Expect::FixCi => models.implement,
            Expect::Review
            | Expect::QuickReview
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
            | Expect::QuickReview
            | Expect::FailureAnalysis
            | Expect::DiffCheck
            | Expect::PackagingGaps => efforts.review,
        };
        Ok((model, effort))
    }

    /// The Local settings for a run of `session`, read afresh so a change
    /// applies from the next run, or the reason the run cannot start. It checks
    /// the server and the model before any checkout or container. A session
    /// resumes on the CLI it started on. A fresh one uses the CLI the job's
    /// requester chose, or the factory default when they chose none or the
    /// factory requested the job.
    fn local_request(
        &self,
        row: &JobRow,
        session: Session,
        model: Option<&str>,
    ) -> Result<Result<ProviderRequest, String>, StoreError> {
        let settings = self.store.local_settings()?;
        let Some(base_url) = settings.url.clone() else {
            return Ok(Err("no local server is set".to_owned()));
        };
        if model.is_none_or(|model| model.trim().is_empty()) {
            return Ok(Err("no model is set for the Local agent".to_owned()));
        }
        let (key, key_from) = match self.local_key(&row.requested_by)? {
            Ok(resolved) => resolved,
            Err(reason) => return Ok(Err(reason)),
        };
        let chosen = if row.requested_by == FACTORY_REQUESTER {
            settings.cli
        } else {
            self.store
                .user_cli(&row.requested_by, Agent::Local)?
                .unwrap_or(settings.cli)
        };
        let cli = self
            .store
            .session_provider_cli(row.id, session)?
            .unwrap_or(chosen);
        Ok(Ok(ProviderRequest {
            target: ProviderTarget {
                kind: ProviderKind::Local {
                    load_timeout: Duration::from_secs(
                        u64::from(settings.load_timeout_minutes) * 60,
                    ),
                },
                cli,
                base_url,
                key,
            },
            key_from,
            setting_cli: chosen,
            session,
        }))
    }

    /// The `OpenRouter` settings for a run of `session`, read afresh, or the
    /// reason the run cannot start. It checks the model, then resolves the key,
    /// then picks the CLI, all before any checkout or container. A session
    /// resumes on the CLI it started on. A fresh one uses the CLI the job's
    /// requester chose, or the factory default when they chose none or the
    /// factory requested the job.
    fn openrouter_request(
        &self,
        row: &JobRow,
        session: Session,
        model: Option<&str>,
    ) -> Result<Result<ProviderRequest, String>, StoreError> {
        if model.is_none_or(|model| model.trim().is_empty()) {
            return Ok(Err("no model is set for the OpenRouter agent".to_owned()));
        }
        let (key, key_from) = match self.openrouter_key(&row.requested_by)? {
            Ok(resolved) => resolved,
            Err(reason) => return Ok(Err(reason)),
        };
        let default = self.store.openrouter_settings()?.cli;
        let chosen = if row.requested_by == FACTORY_REQUESTER {
            default
        } else {
            self.store
                .user_cli(&row.requested_by, Agent::OpenRouter)?
                .unwrap_or(default)
        };
        let cli = self
            .store
            .session_provider_cli(row.id, session)?
            .unwrap_or(chosen);
        Ok(Ok(ProviderRequest {
            target: ProviderTarget {
                kind: ProviderKind::OpenRouter,
                cli,
                base_url: OPENROUTER_BASE_URL.to_owned(),
                key: Some(key),
            },
            key_from,
            setting_cli: chosen,
            session,
        }))
    }

    /// The `OpenRouter` API key a run of a job requested by `requested_by`
    /// uses: the requester's own stored key, except for a job the factory
    /// requested, else the factory key. With neither the run fails, and a key
    /// that cannot be read fails it with no fallback to the other key.
    fn openrouter_key(
        &self,
        requested_by: &str,
    ) -> Result<Result<(String, KeyOwner), String>, StoreError> {
        let secrets = &self.mcp.secrets;
        if requested_by != FACTORY_REQUESTER {
            match secrets.user_openrouter_key(requested_by) {
                Ok(Some(key)) if !key.is_empty() => {
                    return Ok(Ok((key, KeyOwner::Login(requested_by.to_owned()))));
                }
                Ok(_) => {}
                Err(SecretError::Store(err)) => return Err(err),
                Err(err) => {
                    return Ok(Err(format!(
                        "The OpenRouter API key of {requested_by} cannot be read: {err}."
                    )));
                }
            }
        }
        match secrets.openrouter_key() {
            Ok(Some(key)) if !key.is_empty() => Ok(Ok((key, KeyOwner::Factory))),
            Ok(_) => Ok(Err("no OpenRouter API key is set".to_owned())),
            Err(SecretError::Store(err)) => Err(err),
            Err(err) => Ok(Err(format!(
                "The factory's OpenRouter API key cannot be read: {err}."
            ))),
        }
    }

    /// The API key a Local run of a job requested by `requested_by` uses. The
    /// requester's own stored key wins, except for a job the factory itself
    /// requested. Otherwise the factory key applies, or none. A key that cannot
    /// be read fails the run and never falls back to another key.
    fn local_key(
        &self,
        requested_by: &str,
    ) -> Result<Result<(Option<String>, KeyOwner), String>, StoreError> {
        let secrets = &self.mcp.secrets;
        if requested_by != FACTORY_REQUESTER {
            match secrets.user_local_key(requested_by) {
                Ok(Some(key)) if !key.is_empty() => {
                    return Ok(Ok((Some(key), KeyOwner::Login(requested_by.to_owned()))));
                }
                Ok(_) => {}
                Err(SecretError::Store(err)) => return Err(err),
                Err(err) => {
                    return Ok(Err(format!(
                        "The Local API key of {requested_by} cannot be read: {err}."
                    )));
                }
            }
        }
        match secrets.local_key() {
            Ok(Some(key)) if !key.is_empty() => Ok(Ok((Some(key), KeyOwner::Factory))),
            Ok(_) => Ok(Ok((None, KeyOwner::None))),
            Err(SecretError::Store(err)) => Err(err),
            Err(err) => Ok(Err(format!(
                "The factory's Local API key cannot be read: {err}."
            ))),
        }
    }

    fn mcp_config(
        &self,
        job_id: i64,
        project: &str,
        expect: Expect,
    ) -> Result<McpSelection, StoreError> {
        let ActiveServers { active, skipped } = self.active_mcp_servers(project, expect)?;
        let remote: Vec<String> = active
            .iter()
            .filter(|server| matches!(server, ActiveServer::Remote(_)))
            .map(|server| server.name().to_owned())
            .collect();
        let proxy = (!remote.is_empty()).then(|| McpProxy {
            url: self.mcp.proxy_url.clone(),
            token: self.mcp.tokens.issue(job_id, project, &remote),
        });
        let config = (!active.is_empty()).then(|| McpConfig {
            proxy,
            servers: active
                .into_iter()
                .map(|server| match server {
                    ActiveServer::Remote(name) => McpServerEntry::Remote { name },
                    ActiveServer::Stdio {
                        name,
                        command,
                        args,
                    } => McpServerEntry::Stdio {
                        name,
                        command,
                        args,
                    },
                })
                .collect(),
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
                | Expect::QuickReview
                | Expect::FailureAnalysis
                | Expect::DiffCheck
                | Expect::PackagingGaps => settings.stages.review,
            };
            if !(settings.enabled && in_stage) {
                continue;
            }
            if let Some((command, base)) = server.as_stdio() {
                // The project's extra arguments follow the base arguments, read
                // on every run so a change reaches the job's next run.
                let mut args = base.to_vec();
                args.extend(self.store.project_mcp_args(project, &server.name)?);
                active.push(ActiveServer::Stdio {
                    command: command.to_owned(),
                    args,
                    name: server.name,
                });
            } else if self.mcp_secrets_usable(project, &server)? {
                active.push(ActiveServer::Remote(server.name));
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
        let McpTransport::Remote { headers, .. } = &server.transport else {
            return Ok(true);
        };
        for header in headers {
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
        row: &JobRow,
        pr: usize,
        job: &Job,
        project: &Project,
        settings: &Settings,
        workers_free: bool,
    ) -> Result<(), StoreError> {
        let job_id = row.id;
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
        let refs = job_refs(job_id, job, project, pr);
        let (base, existing, branch) = (refs.base, refs.picked, refs.branch);
        let packages = self.store.project_packages(&row.project)?;
        self.spawn(job_id, Kind::Worker, async move {
            scheduler.note(job_id, &format!("Running the checks: {command}"));
            let result = scheduler
                .executor
                .run_checks(
                    &settings,
                    ChecksRequest {
                        job_id,
                        base,
                        existing,
                        branch,
                        command,
                        packages,
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

    fn summarize(
        self: &Arc<Self>,
        job_id: i64,
        pr: usize,
        job: &Job,
        project: &Project,
        settings: &Settings,
    ) {
        let request = SummaryRequest {
            job_id,
            base: job_refs(job_id, job, project, pr).base,
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

    #[allow(clippy::too_many_lines, reason = "one match arm per publish outcome")]
    fn publish(
        self: &Arc<Self>,
        job_id: i64,
        pr: usize,
        job: &Job,
        project: &Project,
        resolved: &Resolved,
        settings: &Settings,
    ) -> Result<(), StoreError> {
        let refs = job_refs(job_id, job, project, pr);
        let existing = (!refs.stacked)
            .then(|| refs.existing(&resolved.forge))
            .flatten();
        let description = match (job.plan()[pr].description.clone(), &existing) {
            (Some(mut description), _) => {
                if let (Some(reviewed), None) = (job.stacked_on(), &existing) {
                    description.body = format!(
                        "{}\n\nFixes review of {}",
                        description.body.trim_end(),
                        reviewed.url
                    );
                }
                description
            }
            (None, Some(_)) => PrDescription {
                title: job.plan()[pr].title.clone(),
                body: job.plan()[pr].summary.clone(),
            },
            (None, None) => {
                self.store.append_factory_log(
                    job_id,
                    "The agent wrote no pull request description, so there is nothing to publish.",
                )?;
                return Ok(());
            }
        };
        let keep_history = match forked_head(job) {
            Ok(keep) => keep,
            Err(reason) => {
                self.apply(job_id, Event::FactoryFailed { reason });
                return Ok(());
            }
        };
        let request = PublishRequest {
            job_id,
            forge: resolved.forge.clone(),
            account: resolved.source.clone(),
            repo: project.repo.clone(),
            base: refs.base,
            branch: refs.branch.clone(),
            existing,
            description,
            checks: project.checks.is_some(),
            keep_history,
            pushed: PushLog::new({
                let scheduler = Arc::clone(self);
                let (branch, repo) = (refs.remote_branch, project.repo.clone());
                move |pushed_as| {
                    scheduler.note(
                        job_id,
                        &format!("Pushed {branch} to {repo} as {pushed_as}."),
                    );
                }
            }),
        };
        let scheduler = Arc::clone(self);
        let settings = settings.clone();
        self.spawn(job_id, Kind::Factory, async move {
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
                Ok(Published::AlreadyClosed(number)) => {
                    scheduler.note(
                        job_id,
                        &format!("Pull request #{number} was closed before the push, so the factory pushed nothing."),
                    );
                    scheduler.apply(job_id, Event::Published { number });
                    scheduler.apply(job_id, Event::ClosedUnmerged);
                }
                Ok(Published::HeadMoved { conflicted }) => {
                    scheduler.note(
                        job_id,
                        if conflicted {
                            "Someone pushed to the pull request first and their commits conflict with the job's, so the agent resolves the merge."
                        } else {
                            "Someone pushed to the pull request first. The factory merged their commits and runs the checks again before it pushes."
                        },
                    );
                    scheduler.apply(job_id, Event::HeadMoved { conflicted });
                }
                Ok(Published::Stopped(reason)) => {
                    scheduler.apply(job_id, Event::FactoryFailed { reason });
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
    /// A command whose `watch` is not [`PrWatch::Full`] marks a watch while
    /// the job waits for the user: the executor then reports only what
    /// [`waiting_event`] allows and takes no other action, so the flags above
    /// do not apply.
    fn watch_pr(
        self: &Arc<Self>,
        row: &JobRow,
        job: &Job,
        project: &Project,
        resolved: &Resolved,
        settings: &Settings,
        command: &Command,
    ) -> Result<(), StoreError> {
        let &Command::WatchPr { pr, number, watch } = command else {
            return Ok(());
        };
        let refs = job_refs(row.id, job, project, pr);
        let request = WatchRequest {
            job_id: row.id,
            forge: resolved.forge.clone(),
            account: resolved.source.clone(),
            repo: project.repo.clone(),
            base: refs.base,
            number,
            seen_until: self.store.pr_watermark(row.id, pr)?,
            auto_merge: self
                .store
                .project_auto_merge(&row.project, project.auto_merge)?,
            may_merge: job.may_merge(),
            llm_review: job.forces_review()
                || self
                    .store
                    .project_llm_review(&row.project, project.llm_review)?,
            review_approved: job.review_state() == ReviewState::Approved,
            watch,
        };
        let scheduler = Arc::clone(self);
        let settings = settings.clone();
        self.spawn(row.id, Kind::Factory, async move {
            scheduler.watch(&settings, pr, request).await;
        });
        Ok(())
    }

    async fn agent(
        &self,
        settings: &Settings,
        request: AgentRequest,
        skipped: Vec<SkippedServer>,
        dispatched: Job,
    ) {
        let job_id = request.job_id;
        // The run's proxy token is revoked when this guard drops: at the end of
        // the run, or when a cancel aborts and drops this future.
        let _token = request
            .mcp
            .as_ref()
            .and_then(|mcp| mcp.proxy.as_ref())
            .map(|proxy| TokenGuard {
                tokens: Arc::clone(&self.mcp.tokens),
                token: proxy.token.clone(),
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
        // The proxy records a call to a remote server. A call to a stdio server
        // never reaches it, so the run's own output is the record.
        let stdio_servers: Vec<String> = request
            .mcp
            .iter()
            .flat_map(|mcp| &mcp.servers)
            .filter_map(|entry| match entry {
                McpServerEntry::Stdio { name, .. } => Some(name.clone()),
                McpServerEntry::Remote { .. } => None,
            })
            .collect();
        let call_warnings = Arc::clone(&self.mcp.warnings);
        let sink: LogSink = Box::new(move |line| {
            if let LogLine::Tool { name, .. } = &line
                && let Some((server, tool)) = stdio_tool_call(name, &stdio_servers)
            {
                call_warnings.record_tool_called(job_id, server, tool);
            }
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
            session_store
                .set_agent_session(&session_uuid, &id)
                .unwrap_or_else(|err| tracing::error!("job {job_id}: {err}"));
        });
        // The model the run passed, kept before the request moves into the run,
        // so a Codex run can look up its price by it afterwards.
        let (model, agent) = (request.model.clone(), request.agent);
        let openrouter_key = openrouter_key_of(&request);
        let report = self
            .executor
            .run_agent(settings, request, sink, session)
            .await;
        self.record_unavailable_servers(job_id, &report);
        self.record_run_cost(
            job_id,
            &cost_session,
            agent,
            model.as_deref(),
            &report,
            openrouter_key,
            dispatched,
        );
        match report.head {
            Some(head) if matches!(report.event, Event::QuickReviewed { .. }) => {
                let Event::QuickReviewed { score, review, .. } = report.event else {
                    unreachable!("matched above");
                };
                self.apply(
                    job_id,
                    Event::QuickReviewed {
                        score,
                        review,
                        head,
                    },
                );
            }
            Some(head)
                if matches!(
                    report.event,
                    Event::ReviewApproved | Event::ReviewChanges { .. }
                ) =>
            {
                let result = self.store.apply_review(job_id, head, report.event);
                self.report(job_id, result.map(|_| ()));
            }
            _ => self.apply(job_id, report.event),
        }
    }

    /// Writes the notices that precede a run's own output, the Local run line
    /// first.
    fn open_run_log(
        &self,
        job_id: i64,
        local: Option<&ProviderRequest>,
        stage_note: Option<&str>,
    ) -> Result<(), StoreError> {
        if let Some(local) = local {
            self.note_local_run(job_id, local);
        }
        if let Some(note) = stage_note {
            self.store.append_factory_log(job_id, note)?;
        }
        Ok(())
    }

    /// Writes the line that opens a Local run: the CLI, the server and whose key
    /// the run uses, never the key. A run that resumes a session on another CLI
    /// than the settings name says so.
    fn note_local_run(&self, job_id: i64, local: &ProviderRequest) {
        let openrouter = local.target.kind == ProviderKind::OpenRouter;
        let key = match &local.key_from {
            KeyOwner::Login(login) if openrouter => {
                format!("the OpenRouter API key of {login}")
            }
            KeyOwner::Login(login) => format!("the Local API key of {login}"),
            KeyOwner::Factory => "the factory's key".to_owned(),
            KeyOwner::None => "no key".to_owned(),
        };
        if openrouter {
            self.note(
                job_id,
                &format!(
                    "OpenRouter run on {} with {key}.",
                    local.target.cli.display_name()
                ),
            );
        } else {
            self.note(
                job_id,
                &format!(
                    "Local run on {} at {} with {key}.",
                    local.target.cli.display_name(),
                    local.target.base_url
                ),
            );
        }
        if local.target.cli != local.setting_cli {
            self.note(
                job_id,
                &format!(
                    "This session started on {from}, so it stays on {from}. The requester's \
                     choice is {to} now, which new sessions use.",
                    from = local.target.cli.display_name(),
                    to = local.setting_cli.display_name(),
                ),
            );
        }
    }

    /// Each active server the agent read as not connected in the run's init
    /// line gets its warning, deduped against a proxy warning for the same
    /// server already recorded during the run.
    fn record_unavailable_servers(&self, job_id: i64, report: &RunReport) {
        for unavailable in &report.mcp_unavailable {
            self.mcp
                .warnings
                .record(job_id, &unavailable.server, &unavailable.reason);
        }
    }

    /// Reads what `OpenRouter` charged for the generations `report` shows with
    /// `key`, in the background so the run's next step never waits for it, and
    /// records the sum. When the cost cannot be read, the run adds no cost and
    /// the log says so.
    fn record_openrouter_cost(
        &self,
        job_id: i64,
        report: &RunReport,
        key: Option<String>,
        at: Job,
    ) {
        let tokens = report.token_usage.map(|usage| {
            format!(
                "{} input tokens, {} output tokens",
                usage.input_tokens, usage.output_tokens
            )
        });
        if let Err(err) = self.store.begin_cost_lookup(job_id) {
            tracing::error!("job {job_id}: {err}");
        }
        let store = Arc::clone(&self.store);
        let costs = Arc::clone(&self.costs);
        let ids = report.generation_ids.clone();
        tokio::spawn(async move {
            let total = match &key {
                Some(key) => costs.total(key, &ids).await,
                None => None,
            };
            let recorded = total.map_or(Ok(None), |total| {
                store
                    .add_fetched_cost(job_id, total, &at)
                    .map(|()| Some(total))
            });
            let mut text = match recorded {
                Ok(Some(total)) => format!(
                    "OpenRouter run finished, {} charged by OpenRouter",
                    format_cost(total)
                ),
                Ok(None) => "OpenRouter run finished. The cost could not be read from \
                             OpenRouter and is not counted"
                    .to_owned(),
                Err(err) => {
                    tracing::error!("job {job_id}: {err}");
                    "OpenRouter run finished. The cost could not be recorded".to_owned()
                }
            };
            if let Some(tokens) = tokens {
                let _ = write!(text, ". {tokens}");
            }
            text.push('.');
            if let Err(err) = store.append_factory_log(job_id, &text) {
                tracing::error!("job {job_id}: {err}");
            }
            if let Err(err) = store.end_cost_lookup(job_id) {
                tracing::error!("job {job_id}: {err}");
            }
        });
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
    /// no cost at all. An `OpenRouter` run records what `OpenRouter` charged for
    /// the generations its output showed, read with `openrouter_key` in the
    /// background, so the run's next step never waits for it.
    #[allow(
        clippy::too_many_arguments,
        reason = "the run's report, key and dispatch snapshot are each needed"
    )]
    fn record_run_cost(
        &self,
        job_id: i64,
        session: &str,
        agent: Agent,
        model: Option<&str>,
        report: &RunReport,
        openrouter_key: Option<String>,
        dispatched: Job,
    ) {
        if agent == Agent::Local {
            // A local model has no price. The run adds a row of zero, so the job
            // reads $0.00 instead of no cost, and ignores the dollar figure the
            // CLI reports and the Codex price table.
            if let Err(err) = self.store.add_cost(job_id, 0.0, false) {
                tracing::error!("job {job_id}: {err}");
            }
            let mut text = "Local run finished, recorded at $0".to_owned();
            if let Some(usage) = report.token_usage {
                let _ = write!(
                    text,
                    ". {} input tokens, {} output tokens",
                    usage.input_tokens, usage.output_tokens
                );
            }
            text.push('.');
            self.note(job_id, &text);
            return;
        }
        if agent == Agent::OpenRouter {
            self.record_openrouter_cost(job_id, report, openrouter_key, dispatched);
            return;
        }
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
        let repo = request.repo.clone();
        self.wait(settings, job_id);
        match self.executor.watch(request).await {
            Ok(report) => {
                self.note_ignored(job_id, number, &repo, &report.ignored);
                let WatchReport {
                    event: Some(event),
                    seen_until,
                    merged_by_factory,
                    ..
                } = report
                else {
                    if let Some(seen) = &report.seen_until {
                        let seeded = self.store.seed_pr_watermark(job_id, pr, seen);
                        self.report(job_id, seeded);
                    }
                    return;
                };
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
            Err(err) => self.retry_later(settings, job_id, &err),
        }
    }

    /// Notes in the job's log why the comments of each ignored login do not
    /// reach the agent, once per job and login.
    fn note_ignored(&self, job_id: i64, number: u64, repo: &str, ignored: &[IgnoredCommenter]) {
        for commenter in ignored {
            let (IgnoredCommenter::NoWrite(login) | IgnoredCommenter::Unknown(login)) = commenter;
            let first = self
                .ignored_noted
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert((job_id, login.clone()));
            if !first {
                continue;
            }
            let text = match commenter {
                IgnoredCommenter::NoWrite(_) => format!(
                    "Ignoring comments on #{number} from {login}, who has no write access to {repo}."
                ),
                IgnoredCommenter::Unknown(_) => format!(
                    "Ignoring comments on #{number} from {login}: the forge does not tell the \
                     job's account whether {login} has write access to {repo}. Forgejo tells \
                     only a repository admin."
                ),
            };
            self.note(job_id, &text);
        }
    }

    async fn watch_renovate(&self, settings: &Settings, request: RenovateWatchRequest) {
        let job_id = request.job_id;
        self.wait(settings, job_id);
        if self.follow_fix(&request).await.is_break() {
            return;
        }
        match self.executor.watch_renovate(request).await {
            Ok(Some(event)) => self.apply(job_id, event),
            Ok(None) => {}
            Err(err) => self.retry_later(settings, job_id, &err),
        }
    }

    /// Reports how the fix job of Renovate job `job_id` ended, when it did.
    /// It reads the fix job on every watch, since a watch of an unchanged
    /// head reports nothing.
    async fn follow_fix(&self, request: &RenovateWatchRequest) -> ControlFlow<()> {
        let job_id = request.job_id;
        let Ok(row) = self.store.get(job_id) else {
            return ControlFlow::Continue(());
        };
        let Some(pr) = row.job.renovate_pr() else {
            return ControlFlow::Continue(());
        };
        let Some(FixLink {
            job: fix_id,
            state: FixState::Running,
            ..
        }) = &pr.fix
        else {
            return ControlFlow::Continue(());
        };
        let fix_id = *fix_id;
        // A reused id belongs to another job, so it counts as deleted.
        let mut outcome = match self.store.get(fix_id) {
            Ok(fix) if fix.job.fix_source() == Some((job_id, pr.number)) => {
                match fix_outcome(&fix.job) {
                    Some(outcome) => outcome,
                    None => return ControlFlow::Continue(()),
                }
            }
            Ok(_) | Err(StoreError::NotFound(_)) => FixOutcome::Deleted,
            Err(err) => {
                tracing::error!("reading fix job #{fix_id}: {err}");
                return ControlFlow::Continue(());
            }
        };
        if matches!(outcome, FixOutcome::Merged { .. }) {
            match self.merge_head(request, fix_id).await {
                Ok(merged) => outcome = merged,
                Err(err) => {
                    if let Some(reason) = credential_reason(&err) {
                        self.apply(
                            job_id,
                            Event::FactoryFailed {
                                reason: reason.to_owned(),
                            },
                        );
                        return ControlFlow::Break(());
                    }
                    tracing::warn!("reading the head of #{}: {err}", request.number);
                    return ControlFlow::Continue(());
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
        ControlFlow::Continue(())
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
                .merged_at(&request.forge, &request.account, &request.repo, number)
                .await?;
            merged_at = merged_at.max(at);
        }
        let merged_at = merged_at.ok_or("the forge shows no merged fix pull request")?;
        let head = self
            .executor
            .renovate_head(
                &request.forge,
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
            .request_rebase(&resolved.forge, &resolved.source, repo, number, head)
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
            Err(err) if credential_reason(&err).is_some() => {
                // The forge refused the credential, so nothing was written and Retry may ask again.
                if let Err(err) = self.store.release_rebase_claim(job_id) {
                    tracing::error!("releasing the rebase claim of job #{job_id}: {err}");
                }
                Event::FactoryFailed {
                    reason: credential_reason(&err).unwrap_or_default().to_owned(),
                }
            }
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

    /// One account for the lookup, the saved login and the POST of a quick
    /// review's comment, so a switch during the step cannot split them. A
    /// failure is recorded as `CommentFailed` and gives `None`.
    fn pinned_source(
        &self,
        job_id: i64,
        resolved: &Resolved,
        number: u64,
    ) -> Option<AccountSource> {
        match resolved.source.resolve() {
            Ok(operation) => Some(AccountSource::new(move || Ok(operation.clone()))),
            Err(err) => {
                self.note(job_id, &format!("Could not comment on #{number}: {err}"));
                self.apply(job_id, Event::CommentFailed { error: err });
                None
            }
        }
    }

    fn comment_failed(&self, job_id: i64, number: u64, error: String) {
        self.note(job_id, &format!("Could not comment on #{number}: {error}"));
        self.apply(job_id, Event::CommentFailed { error });
    }

    /// The login that posted or was about to post the comment, or the pinned
    /// account's when there was no earlier attempt.
    fn saved_author(&self, job_id: i64, source: &AccountSource) -> Result<String, String> {
        match self
            .store
            .comment_author(job_id)
            .map_err(|err| err.to_string())?
        {
            Some(login) => Ok(login),
            None => Ok(source.resolve()?.account.login),
        }
    }

    /// Saves the pinned account's login before the POST.
    fn save_author(&self, job_id: i64, source: &AccountSource) -> Result<(), String> {
        let login = source.resolve()?.account.login;
        self.store
            .set_comment_author(job_id, &login)
            .map_err(|err| err.to_string())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "each argument is a separate fact of the operation that starts"
    )]
    async fn post_comment(
        &self,
        settings: &Settings,
        job_id: i64,
        resolved: &Resolved,
        repo: &str,
        number: u64,
        body: &str,
        quick_review: bool,
    ) {
        let full;
        let pinned;
        let source = if quick_review {
            let Some(source) = self.pinned_source(job_id, resolved, number) else {
                return;
            };
            pinned = source;
            &pinned
        } else {
            &resolved.source
        };
        let body = if quick_review {
            // The marker holds the review session's id, which is stored with
            // the job. A restart that lost the saved result finds the comment
            // the account already posted, and only that comment counts.
            let session = match self
                .store
                .session_uuid(job_id, Session::Review(0), DEFAULT_AGENT)
            {
                Ok((session, _)) => session,
                Err(err) => {
                    self.note(job_id, &format!("Could not comment on #{number}: {err}"));
                    self.apply(
                        job_id,
                        Event::CommentFailed {
                            error: err.to_string(),
                        },
                    );
                    return;
                }
            };
            full = format!("{body}\n\n{}", quick_review_marker(&session));
            // The account that may already have posted is the one recorded
            // before the first attempt, which a switch of the job's account
            // does not change.
            let author = match self.saved_author(job_id, source) {
                Ok(author) => author,
                Err(err) => {
                    self.comment_failed(job_id, number, err);
                    return;
                }
            };
            match self
                .executor
                .has_comment(&resolved.forge, source, &author, repo, number, &full)
                .await
            {
                Ok(true) => {
                    self.note(job_id, &format!("Already commented on #{number}."));
                    self.apply(job_id, Event::Commented);
                    return;
                }
                Ok(false) => {}
                Err(err) => {
                    self.note(job_id, &format!("Could not comment on #{number}: {err}"));
                    self.apply(job_id, Event::CommentFailed { error: err });
                    return;
                }
            }
            if let Err(err) = self.save_author(job_id, source) {
                self.comment_failed(job_id, number, err);
                return;
            }
            &full
        } else {
            body
        };
        match self
            .executor
            .comment(&resolved.forge, source, repo, number, body)
            .await
        {
            Ok(()) => {
                self.note(job_id, &format!("Commented on #{number}: {body}"));
                self.apply(job_id, Event::Commented);
            }
            // A quick review's comment is optional, so the review stays on the
            // job page with the forge's error instead of waiting to retry.
            Err(err) if quick_review => {
                self.note(job_id, &format!("Could not comment on #{number}: {err}"));
                self.apply(job_id, Event::CommentFailed { error: err });
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
            .merge_renovate(&resolved.forge, &resolved.source, repo, number, head)
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
                // merged. The merge is recorded first, so a credential failure
                // that stops the job leaves Retry resuming the watch and not
                // the merge.
                match self
                    .executor
                    .renovate_merged(
                        &resolved.forge,
                        &resolved.source,
                        repo,
                        number,
                        manifest_files.as_deref(),
                    )
                    .await
                {
                    Ok(event) => self.apply(job_id, event),
                    Err(err) => {
                        self.apply(job_id, Event::MergeFailed);
                        self.retry_later(settings, job_id, &err);
                    }
                }
            }
            Err(err) if err.starts_with(NOT_PERMITTED) => {
                self.note(job_id, &format!("Did not merge #{number}: {err}"));
                self.apply(job_id, Event::MergeFailed);
            }
            Err(err) => {
                // A credential failure stops the job with the merge pending,
                // so Retry merges again. The merge event only applies to a job
                // that is still running.
                let credentials = credential_reason(&err).is_some();
                self.retry_later(settings, job_id, &err);
                if !credentials {
                    self.apply(job_id, Event::MergeFailed);
                }
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
            Err(err) if renovate && err.starts_with(NOT_PERMITTED) => {
                self.note(job_id, &format!("Did not rerun CI on #{number}: {err}"));
                self.apply(job_id, Event::RerunDenied);
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

    /// Handles a failed forge or git step. A failure of the credentials fails
    /// the job with the reason, so the job shows Retry and the step resumes
    /// once a token is saved. Any other failure is tried again after the poll
    /// interval.
    fn retry_later(&self, settings: &Settings, job_id: i64, err: &str) {
        if let Some(reason) = credential_reason(err) {
            self.apply(
                job_id,
                Event::FactoryFailed {
                    reason: reason.to_owned(),
                },
            );
            return;
        }
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
