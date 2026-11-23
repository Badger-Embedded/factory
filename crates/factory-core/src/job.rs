use std::fmt;

use serde::{Deserialize, Serialize};

/// Check failures, CI failures and merge conflicts a job may hit on one
/// pull request before it fails. A conflict counts, so an agent that cannot
/// resolve one does not rebase forever.
pub const MAX_FAILED_ROUNDS: u32 = 5;

/// Changes-requested LLM review passes a job may take on one pull request
/// before it stops and waits for the user. Separate from
/// [`MAX_FAILED_ROUNDS`], so a job can spend both on the same pull request.
pub const MAX_REVIEW_ROUNDS: u32 = 5;

/// The failure reason a job carried, before the retry target existed, when
/// it used up its failed rounds. A failed job with this exact reason and no
/// retry target is a legacy job, retryable at its highest-numbered pull
/// request. See [`Job::can_retry`].
const LEGACY_ROUNDS_REASON: &str = "5 failed check or CI rounds on one pull request";

/// How the reason of a failed agent run starts, such as a run stopped by the
/// plan's usage limit. A failed job with this prefix and no retry target
/// failed before agent failures recorded one, and is retryable at its
/// highest-numbered pull request like a [`LEGACY_ROUNDS_REASON`] job.
const LEGACY_AGENT_REASON: &str = "the agent run ended with";

/// The reason a review stopped with [`ReviewStop::Unchanged`].
const UNCHANGED_REASON: &str = "The author added no commit after the last review asked for \
     changes, and you answered no question since, so another pass would give the same \
     verdict. Merge the pull request on GitHub to accept it as it is, or retry to run the \
     reviewer again.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    request: String,
    stage: Stage,
    turn: Turn,
    spec: Option<String>,
    plan: Vec<PlannedPr>,
    failed_rounds: u32,
    #[serde(default)]
    gates: Gates,
    /// The answers to the agent's questions, in the order given.
    #[serde(default)]
    answers: Vec<Answered>,
    /// Whether the spec and plan runs start their own sessions with a
    /// self-contained prompt. A job stored before this field existed loads
    /// with `false` and keeps resuming its planning session.
    #[serde(default)]
    fresh_sessions: bool,
    /// Changes-requested LLM review passes on the current pull request. Reset
    /// with the failed rounds when a pull request starts. Caps at
    /// [`MAX_REVIEW_ROUNDS`].
    #[serde(default)]
    review_rounds: u32,
    /// Whether the reviewer has approved the pull request's current head. Set
    /// to [`ReviewState::Pending`] again on every push, so each head is
    /// reviewed afresh.
    #[serde(default)]
    review: ReviewState,
    /// One record per review pass, in order, kept for the job page.
    #[serde(default)]
    reviews: Vec<ReviewPass>,
    /// The head commit the running review pass reads, from
    /// [`Event::StartReview`]. Recorded on the pass when it ends.
    #[serde(default)]
    review_head: Option<String>,
    /// The plan indexes of the pull requests the factory skipped because
    /// their branch had no commits to publish. See [`Event::NothingToPublish`].
    #[serde(default)]
    skipped: Vec<usize>,
    /// What the job works on. A job stored before this field existed loads as
    /// [`JobKind::Request`].
    #[serde(default)]
    kind: JobKind,
}

/// What a job works on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobKind {
    /// A feature request that goes through the clarify, spec, plan, implement
    /// and publish stages.
    #[default]
    Request,
    /// A Renovate pull request the factory watches. It skips every stage of a
    /// request and ends when the pull request merges or closes.
    Renovate(Box<RenovatePr>),
    /// A packaging gap the factory found after merging Renovate pull request
    /// `source_pr`, found by job `source_job`. It waits for approval at
    /// [`Stage::Proposed`], then runs like a request.
    Gap { source_job: i64, source_pr: u64 },
}

/// How many times the factory reruns failed CI on one head commit of a
/// Renovate pull request. The factory reads them on every watch and passes
/// them in [`Event::RenovateObserved`], so a saved change reaches a job in
/// flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryLimits {
    /// Reruns with no agent run, before an agent analyses the failure.
    pub auto: u32,
    /// All reruns on one head commit, before the factory stops.
    pub total: u32,
}

/// The comments the factory posts on a Renovate pull request, each at most
/// once per head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommentKind {
    /// The failure analysis found that the update causes the failure.
    CausedByUpdate,
    /// The diff check found the update unsafe to merge.
    Unsafe,
    /// CI fails on a check with no failed Actions run to rerun.
    NothingToRerun,
    /// CI still fails after the total reruns.
    Stopped,
}

/// The diff check's verdict on one head commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenovateVerdict {
    pub head: String,
    pub safe: bool,
    pub reason: String,
    pub summary: String,
    /// The upstream refs the update moves between, when the diff shows them.
    pub from: Option<String>,
    pub to: Option<String>,
}

/// What the factory read from a pull request whose checks all passed, for the
/// diff check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDiff {
    pub diff: String,
    pub body: String,
}

/// A packaging gap: something an upstream update introduced that the
/// packaging does not reflect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub title: String,
    pub description: String,
    /// What upstream changed, quoted from its diff or release notes.
    pub evidence: String,
    /// What the packaging has now.
    pub current: String,
    pub suggestion: String,
}

impl Gap {
    /// The request text of the job that implements the gap. It starts with
    /// the title, so a later analysis can tell what the job covers. `range`
    /// is the upstream refs the update moved between, when known.
    #[must_use]
    pub fn request(&self, number: u64, range: Option<(&str, &str)>) -> String {
        let mut text = format!(
            "{}\n\n{}\n\nUpstream evidence:\n\n{}\n\nCurrent packaging:\n\n{}\n\n\
             Suggested change:\n\n{}\n\nFound after Renovate pull request #{number} merged",
            self.title, self.description, self.evidence, self.current, self.suggestion
        );
        if let Some((from, to)) = range {
            text.push_str(", moving upstream from ");
            text.push_str(from);
            text.push_str(" to ");
            text.push_str(to);
        }
        text.push('.');
        text
    }
}

/// A file the packaging analysis reads. `text` is `None` when the file does
/// not exist at the ref read, or, for an upstream patch, did not change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileText {
    pub path: String,
    pub text: Option<String>,
}

/// A gap job the project already has, so the analysis leaves out what it
/// covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingGap {
    pub request: String,
    /// The user dismissed the job. Otherwise it is proposed or in progress.
    pub dismissed: bool,
}

/// What the packaging analysis of a merged Renovate pull request reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackagingContext {
    /// The packaging type: `snap`, `deb` or `other`.
    pub kind: String,
    pub upstream_repo: String,
    pub pr_title: String,
    pub pr_body: String,
    pub pr_diff: String,
    /// The manifest files as they are on the base branch after the merge.
    pub manifests: Vec<FileText>,
    /// The upstream refs the update moved between. `None` when the diff
    /// check never ran, because a person merged first.
    pub from: Option<String>,
    pub to: Option<String>,
    /// The upstream patch of each configured build config file.
    pub upstream: Vec<FileText>,
    pub existing: Vec<ExistingGap>,
}

/// The state of a Renovate pull request's checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenovateCi {
    /// No check has reported.
    NoChecks,
    Pending,
    Failed {
        log: String,
    },
    /// Every check passed. `details` is what the diff check reads, and the
    /// factory fetches it only when the job has no verdict for the head.
    Passed {
        details: Option<Box<PrDiff>>,
    },
}

/// Why a Renovate job waits for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenovateWait {
    /// Triage stopped: CI still fails after the total reruns, or the update
    /// causes the failure. Retry resumes it.
    Stopped,
    /// The update is safe and every check passed, but auto-merge is off.
    Ready,
    /// The diff check found the update unsafe.
    Unsafe,
}

/// What the factory does for a Renovate job before it watches again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Pending {
    /// Post `body`, then wait for the user with `then`.
    Comment {
        kind: CommentKind,
        body: String,
        then: Ask,
    },
    /// Approve, then merge the recorded head.
    Merge,
    /// Create a job for each gap, then finish.
    Propose {
        gaps: Vec<Gap>,
        range: Option<(String, String)>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum RerunKind {
    #[default]
    Auto,
    Analysed,
    Retry,
}

/// A Renovate pull request and the factory's triage state for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenovatePr {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The head commit the counters and the verdict belong to.
    pub head: Option<String>,
    /// All reruns on `head`.
    pub reruns: u32,
    /// Reruns on `head` that needed no agent run.
    pub auto_reruns: u32,
    pub verdict: Option<RenovateVerdict>,
    /// Why triage stopped on `head`.
    pub stopped: Option<String>,
    /// The comments posted so far, as `(head, kind)`.
    pub comments: Vec<(String, CommentKind)>,
    /// The agent runs started so far. Each run has its own session.
    #[serde(default)]
    pub runs: u32,
    /// The gap jobs the packaging analysis created, or `None` while it has
    /// not run. An empty list means it found no gaps.
    #[serde(default)]
    pub gaps: Option<Vec<i64>>,
    #[serde(default)]
    pending: Option<Pending>,
    #[serde(default)]
    rerun: RerunKind,
}

impl RenovatePr {
    fn commented(&self, kind: CommentKind) -> bool {
        self.head
            .as_ref()
            .is_some_and(|head| self.comments.iter().any(|(h, k)| h == head && *k == kind))
    }

    fn reset(&mut self, head: String) {
        self.head = Some(head);
        self.reruns = 0;
        self.auto_reruns = 0;
        self.verdict = None;
        self.stopped = None;
        self.pending = None;
        self.rerun = RerunKind::Auto;
    }

    fn stop(&mut self, kind: CommentKind, reason: String, body: String) -> Turn {
        let state = if kind == CommentKind::Unsafe {
            RenovateWait::Unsafe
        } else {
            self.stopped = Some(reason.clone());
            RenovateWait::Stopped
        };
        let then = Ask::Renovate { reason, state };
        if self.commented(kind) {
            return Turn::User(then);
        }
        self.pending = Some(Pending::Comment { kind, body, then });
        Turn::Factory
    }

    fn on_failure_verdict(&mut self, transient: bool, reason: &str) -> Turn {
        if transient {
            self.rerun = RerunKind::Analysed;
            return Turn::RerunCi {
                reason: format!("transient failure: {reason}"),
            };
        }
        let body =
            format!("CI fails because of this update, so rerunning it will not help.\n\n{reason}");
        let reason = format!("CI fails because of the update: {reason}");
        self.stop(CommentKind::CausedByUpdate, reason, body)
    }

    /// Records the verdict for the current head. A safe update goes back to
    /// watching, where the next observation merges it or waits for the user.
    fn on_diff_verdict(&mut self, found: DiffVerdict) -> Turn {
        let DiffVerdict {
            safe,
            reason,
            summary,
            from,
            to,
        } = found;
        self.verdict = Some(RenovateVerdict {
            head: self.head.clone().unwrap_or_default(),
            safe,
            reason: reason.clone(),
            summary,
            from,
            to,
        });
        if safe {
            return Turn::Factory;
        }
        let (reason, body) = unsafe_texts(&reason);
        self.stop(CommentKind::Unsafe, reason, body)
    }

    /// The rules for one observation of the pull request, after a new head
    /// has reset the counters. The rerun check on the total limit comes
    /// first, so a lowered limit stops a job on its next watch.
    fn decide(&mut self, ci: RenovateCi, limits: RetryLimits, auto_merge: bool) -> Turn {
        match ci {
            RenovateCi::NoChecks | RenovateCi::Pending => Turn::Factory,
            RenovateCi::Failed { log } => {
                if self.reruns >= limits.total {
                    let head = self.head.as_deref().unwrap_or_default();
                    let short = &head[..head.len().min(7)];
                    let reruns = self.reruns;
                    self.stop(
                        CommentKind::Stopped,
                        format!("CI still fails after {reruns} reruns on this head commit."),
                        format!(
                            "CI still fails on {short} after {reruns} reruns. The factory stops \
                             rerunning it and leaves this pull request to a person."
                        ),
                    )
                } else if self.auto_reruns < limits.auto {
                    self.rerun = RerunKind::Auto;
                    Turn::RerunCi {
                        reason: format!(
                            "automatic rerun {} of {}",
                            self.auto_reruns + 1,
                            limits.auto
                        ),
                    }
                } else {
                    self.runs += 1;
                    Turn::Agent(Task::AnalyzeFailure { log })
                }
            }
            RenovateCi::Passed { details } => match (&self.verdict, details) {
                (Some(verdict), _) if !verdict.safe => {
                    let (reason, body) = unsafe_texts(&verdict.reason);
                    self.stop(CommentKind::Unsafe, reason, body)
                }
                (Some(_), _) if auto_merge => {
                    self.pending = Some(Pending::Merge);
                    Turn::Factory
                }
                (Some(_), _) => Turn::User(Ask::Renovate {
                    reason: "Every check passed and the diff check found the update safe. \
                             Merge it on GitHub, or turn on auto-merge for the project."
                        .to_owned(),
                    state: RenovateWait::Ready,
                }),
                (None, Some(details)) => {
                    self.runs += 1;
                    Turn::Agent(Task::CheckDiff {
                        diff: details.diff,
                        body: details.body,
                    })
                }
                (None, None) => Turn::Factory,
            },
        }
    }
}

fn unsafe_texts(reason: &str) -> (String, String) {
    (
        format!("The diff check found the update unsafe to merge: {reason}"),
        format!(
            "The factory does not merge this update, because its diff does more than change \
             dependency or packaging version files.\n\n{reason}"
        ),
    )
}

/// Whether the reviewer has approved the current head of a pull request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    /// The current head has not been reviewed, or a push replaced the head the
    /// reviewer approved.
    #[default]
    Pending,
    /// The reviewer approved the current head.
    Approved,
}

/// One LLM review pass on a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPass {
    /// The pass number, counting every pass on the job from one.
    pub number: u32,
    pub verdict: Verdict,
    /// The reviewer's feedback, set when it asked for changes.
    pub feedback: Option<String>,
    /// The index in the plan of the pull request the pass reviewed. `None`
    /// for a pass stored before this field existed.
    #[serde(default)]
    pub pr: Option<usize>,
    /// The head commit the pass reviewed. `None` for a pass stored before
    /// this field existed.
    #[serde(default)]
    pub head: Option<String>,
    /// How many of the requester's answers the job held when the pass ran,
    /// so a later pass gets only the answers given since.
    #[serde(default)]
    pub answers_seen: usize,
}

/// What a review pass decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Approve,
    Changes,
}

/// A question the agent asked and the requester's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answered {
    pub question: String,
    pub answer: String,
}

/// Which steps wait for the user. A job stored without gates loads with
/// `spec` and `plan` on and `publish` off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gates {
    /// Wait for the user to approve the spec.
    pub spec: bool,
    /// Wait for the user to approve the plan.
    pub plan: bool,
    /// Wait for the user to approve a pull request before its first push.
    pub publish: bool,
}

impl Default for Gates {
    fn default() -> Self {
        Gates {
            spec: true,
            plan: true,
            publish: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Requested,
    Clarifying,
    Spec,
    Plan,
    Implementing {
        pr: usize,
    },
    Verifying {
        pr: usize,
    },
    PrOpen {
        pr: usize,
    },
    CiAndReview {
        pr: usize,
    },
    /// Watching a Renovate pull request. See [`JobKind::Renovate`].
    Renovate,
    /// The packaging analysis of a merged Renovate pull request. See
    /// [`JobKind::Renovate`].
    Packaging,
    /// A gap job waits for the user to approve or dismiss it. It holds no
    /// worker and runs no agent. See [`JobKind::Gap`].
    Proposed,
    /// The user dismissed a gap job at [`Stage::Proposed`]. It never ran.
    Dismissed,
    Merged,
    Cancelled,
    Failed {
        reason: String,
        /// Where a retry resumes the job, set when the job ran out of failed
        /// rounds or an agent run failed. A job that failed another way, or
        /// one stored before this field existed, has `None`. See
        /// [`Job::can_retry`].
        #[serde(default)]
        retry: Option<RetryTarget>,
        /// Whether the user dismissed the job from the Failed section of the
        /// board. A dismissed job stays failed, with its reason and retry
        /// target unchanged, and shows in Done. A job stored before this
        /// field existed loads as not dismissed. See [`Event::Dismiss`].
        #[serde(default)]
        dismissed: bool,
    },
}

/// Where a retried job resumes. A job that used up its failed rounds returns
/// to the pull request it failed on. A job whose agent run failed runs the
/// same task again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetryTarget {
    /// Rerun the project's checks on the pull request.
    Verifying { pr: usize },
    /// Watch the pull request's CI and review again.
    CiAndReview { pr: usize },
    /// Restore the stage and turn a factory step was interrupted in.
    Resume { stage: Box<Stage>, turn: Box<Turn> },
    /// Run `task` again in `stage`, the stage the failed run belonged to.
    /// The stage picks the session, so the run resumes the session that
    /// failed, in the same checkout.
    Agent { stage: Box<Stage>, task: Box<Task> },
}

impl RetryTarget {
    fn resume(stage: &Stage, turn: &Turn) -> Self {
        Self::Resume {
            stage: Box::new(stage.clone()),
            turn: Box::new(turn.clone()),
        }
    }
}

/// Who the job is waiting for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Turn {
    Agent(Task),
    Factory,
    /// The factory reruns the pull request's failed CI runs. The agent asked
    /// for it and gave `reason`.
    RerunCi {
        reason: String,
    },
    User(Ask),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    Answers {
        questions: Vec<Question>,
    },
    SpecApproval,
    PlanApproval,
    /// The user approves the first push of a pull request. The summary is
    /// `None` until the factory has collected it.
    PublishApproval {
        summary: Option<BranchSummary>,
    },
    /// The review stopped without an approval: it used up its rounds or a
    /// reviewer run failed. `reason` says which in words; `stop` records the
    /// case for the retry, so the retry never parses `reason`. The job does
    /// not merge and waits for the user.
    ReviewStopped {
        reason: String,
        /// Why the review stopped, which decides where a retry resumes. A job
        /// stored before this field existed, like #28, loads with the
        /// default and retries as a reviewer-run failure.
        #[serde(default)]
        stop: ReviewStop,
    },
    /// A gap job waits for the user to approve or dismiss it.
    Proposal,
    /// A Renovate job waits for the user. `reason` says why in words.
    Renovate {
        reason: String,
        state: RenovateWait,
    },
}

/// Why a review stopped, which a [`Event::Retry`] uses to resume the job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStop {
    /// A reviewer run failed: usage limit, crash, timeout, or missing or
    /// invalid output. Retry reruns the reviewer in the same review session,
    /// leaving the review and failed round counts as they are. The default,
    /// so a job stored before the field existed retries this way.
    #[default]
    ReviewerFailed,
    /// The review reached [`MAX_REVIEW_ROUNDS`]. Retry clears the review
    /// rounds and sends `feedback`, the last pass's, to the implement session
    /// as [`Task::AddressReview`], giving the pull request five fresh rounds.
    CapReached { feedback: String },
    /// CI went green on the head the last pass asked to change, and the
    /// requester answered no question since. The author added no commit, so
    /// a new pass would read the same diff and give the same verdict. Retry
    /// runs the reviewer on that head anyway.
    Unchanged,
}

/// What a pull request branch would add to its base branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSummary {
    /// One `<short sha> <subject>` entry per commit.
    pub commits: Vec<String>,
    /// The `git diff --stat` output. Empty when the branch changes nothing.
    pub diff_stat: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPr {
    pub title: String,
    pub summary: String,
    /// The GitHub pull request number, once the factory has opened it.
    pub number: Option<u64>,
    /// The title and description the agent wrote with its latest commits.
    #[serde(default)]
    pub description: Option<PrDescription>,
    /// What the planning run found that the implement run would otherwise
    /// search for again: the files and functions to change, the tests to
    /// extend and the design rows that apply. `None` for a plan written
    /// before this field existed.
    #[serde(default)]
    pub notes: Option<String>,
}

/// A pull request's title and description, written by the agent to the
/// target repository's rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDescription {
    pub title: String,
    pub body: String,
}

/// The input for one agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Task {
    Clarify {
        request: String,
    },
    WriteSpec,
    WritePlan,
    Implement {
        pr: PlannedPr,
    },
    Answer {
        questions: Vec<Question>,
        answers: Vec<String>,
    },
    Revise {
        feedback: String,
    },
    FixChecks {
        report: String,
    },
    FixCi {
        log: String,
    },
    AddressReview {
        comments: String,
    },
    /// Rebase the pull request's branch onto the newest `origin/<base>`,
    /// which the factory fetched before the run.
    Rebase {
        base: String,
    },
    /// Review the pull request's diff and end with `approve` or `changes`. The
    /// worker gets no GitHub token and reads the diff from the checkout.
    Review,
    /// Read the log of failed CI on a Renovate pull request, whose automatic
    /// reruns are used up, and decide whether the failure is transient. The
    /// worker gets no GitHub token and the factory passes the log.
    AnalyzeFailure {
        log: String,
    },
    /// Decide whether a Renovate pull request's diff changes only dependency
    /// or packaging version files. The factory passes the diff and the
    /// description.
    CheckDiff {
        diff: String,
        body: String,
    },
    /// Compare the upstream build configuration with the packaging manifest
    /// after a merged Renovate pull request. The factory passes everything
    /// the run reads.
    FindGaps {
        context: Box<PackagingContext>,
    },
}

/// The agent session a run uses. Clarifying has one, the spec has one, and
/// so does the plan. Each planned pull request has its own. A job created
/// before fresh sessions keeps one `Planning` session for clarifying, spec
/// and plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Session {
    Planning,
    Spec,
    Plan,
    Pr(usize),
    /// The LLM review of a pull request. Separate from the implement session,
    /// so the reviewer reads the diff without the author's context. It resumes
    /// across passes, so a later pass sees only the commits since the last one.
    Review(usize),
    /// A run on a Renovate pull request, numbered by `RenovatePr::runs`.
    Renovate(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    RunAgent {
        session: Session,
        task: Task,
    },
    RunChecks {
        pr: usize,
    },
    /// Collect the branch's commits and diff stat for the publish approval.
    Summarize {
        pr: usize,
    },
    /// Push the pull request's branch and open the pull request if it is not
    /// open yet.
    Publish {
        pr: usize,
    },
    WatchPr {
        pr: usize,
        number: u64,
        /// Report only a merge or close by someone else, and take no other
        /// action: do not merge, start a review, or send CI failures and
        /// comments to the agent. Set while the review has stopped and waits
        /// for the user, so the job moves on when the user merges the pull
        /// request on GitHub and fails when they close it, but never merges or
        /// restarts the reviewer on its own.
        watch_only: bool,
    },
    /// Rerun the failed CI runs of the pull request. The factory logs
    /// `reason`.
    RerunCi {
        pr: usize,
        number: u64,
        reason: String,
    },
    /// Watch a Renovate pull request. `verdict_head` is the head the job has
    /// a diff verdict for, so the factory fetches the diff only for another
    /// head. With `watch_only` the factory reports an observation only when
    /// the head changed, besides a merge or close.
    WatchRenovate {
        number: u64,
        verdict_head: Option<String>,
        watch_only: bool,
    },
    /// Rerun the failed CI runs of a Renovate pull request. The factory logs
    /// `reason`.
    RerunRenovate {
        number: u64,
        reason: String,
    },
    /// Post `body` as a comment on the pull request, then report
    /// [`Event::Commented`].
    PostComment {
        number: u64,
        body: String,
    },
    /// Approve the pull request and squash-merge `head`, and only that head.
    MergeRenovate {
        number: u64,
        head: String,
    },
    /// Read what the packaging analysis of the merged pull request needs,
    /// then report [`Event::PackagingGathered`].
    GatherPackaging {
        number: u64,
    },
    /// Create a job for each gap and report [`Event::GapsProposed`], in one
    /// store transaction.
    ProposeGaps {
        gaps: Vec<Gap>,
        /// The upstream versions the analysis compared, for the request text.
        range: Option<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentResult {
    Questions(Vec<Question>),
    Clarified,
    Spec(String),
    Plan(Vec<PlannedPr>),
    CommitsReady(PrDescription),
    /// The agent has nothing to commit and wants CI to run again, for
    /// example after a flaky failure. `reason` says why.
    RerunCi {
        reason: String,
    },
    /// The failure analysis of a Renovate pull request.
    FailureVerdict {
        /// The failure has nothing to do with the update, so a rerun may pass.
        transient: bool,
        reason: String,
    },
    /// The diff check of a Renovate pull request.
    DiffVerdict(Box<DiffVerdict>),
    /// The packaging analysis: the gaps it found, possibly none.
    Gaps(Vec<Gap>),
}

/// What the diff check of a Renovate pull request returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffVerdict {
    pub safe: bool,
    pub reason: String,
    pub summary: String,
    /// The upstream refs the update moves between, when the diff shows them.
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Start,
    /// Replace the request text of a job still queued in `requested`. Applies
    /// only at `(Requested, Factory)`, before the scheduler starts the job.
    Edit {
        request: String,
    },
    Agent(AgentResult),
    AgentFailed {
        reason: String,
    },
    /// The factory cannot run the step it owes, so the job fails with `reason`.
    /// A retry resumes the step where the job has a retry target. Applies to
    /// every unfinished job that is not waiting for an agent run.
    FactoryFailed {
        reason: String,
    },
    Answer(Vec<String>),
    Approve,
    RequestChanges {
        feedback: String,
    },
    ChecksPassed,
    /// The factory collected what the publish approval shows.
    Summarized(BranchSummary),
    ChecksFailed {
        report: String,
    },
    Published {
        number: u64,
    },
    CiFailed {
        log: String,
    },
    /// The factory reran the failed CI runs the agent asked to rerun.
    CiRerun,
    /// The failed check of a Renovate pull request has no Actions run to
    /// rerun, so no rerun happened and no count changes.
    NothingToRerun,
    /// The approval or the merge of a Renovate head failed. The job goes back
    /// to watching, so the next look sees the current head and state.
    MergeFailed,
    /// The pull request no longer merges cleanly into `base`.
    Conflicted {
        base: String,
    },
    ReviewComments {
        comments: String,
    },
    /// CI is green on `head` and the project has LLM review on, so the
    /// reviewer should run before the factory merges.
    StartReview {
        head: String,
    },
    /// The reviewer approved the pull request's current head.
    ReviewApproved,
    /// The reviewer asked for changes and gave `feedback`.
    ReviewChanges {
        feedback: String,
    },
    /// The reviewer run failed: it crashed, timed out, or returned no valid
    /// result. The job fails closed and does not merge.
    ReviewFailed {
        reason: String,
    },
    /// The factory watched a Renovate pull request. `head` is its head
    /// commit, `limits` the retry limits now saved and `auto_merge` the
    /// project's switch now.
    RenovateObserved {
        head: String,
        ci: RenovateCi,
        limits: RetryLimits,
        auto_merge: bool,
    },
    /// The factory posted the comment of [`Command::PostComment`].
    Commented,
    Merged,
    /// A Renovate pull request merged. `packaging` says the project is a
    /// packaging project and `manifest_changed` that the pull request changed
    /// a configured manifest file. Only both start the packaging analysis.
    RenovateMerged {
        packaging: bool,
        manifest_changed: bool,
    },
    /// The factory read what the packaging analysis needs.
    PackagingGathered(Box<PackagingContext>),
    /// The factory created a job for each gap, listed by id.
    GapsProposed {
        jobs: Vec<i64>,
    },
    ClosedUnmerged,
    /// The pull request's branch has no commits after the base branch, so
    /// there is nothing to push. This happens when an earlier pull request
    /// already made the change. A pull request with no number is skipped and
    /// the job moves on to the next one. An open one fails the job, since
    /// pushing the empty branch would close it.
    NothingToPublish,
    Cancel,
    /// Resume a job that failed because it used up its failed rounds or an
    /// agent run failed. See [`Job::can_retry`].
    Retry,
    /// Clear a failed job off the Failed section of the board without
    /// deleting it. Sets the dismissed flag on a failed job that is not yet
    /// dismissed; every other stage, and an already dismissed job, reject it.
    /// The job stays failed, so its reason, retry target and directories are
    /// unchanged. See [`Stage::Failed`].
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    /// The event does not apply to the job's current stage and turn. A
    /// duplicate or late event ends here.
    Unexpected {
        stage: Stage,
        event: Event,
    },
    AnswerCount {
        expected: usize,
        got: usize,
    },
    EmptyPlan,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejected::Unexpected { stage, event } => {
                write!(f, "event {event:?} does not apply in stage {stage:?}")
            }
            Rejected::AnswerCount { expected, got } => {
                write!(f, "{got} answers for {expected} questions")
            }
            Rejected::EmptyPlan => write!(f, "the plan has no pull requests"),
        }
    }
}

impl std::error::Error for Rejected {}

impl Job {
    #[must_use]
    pub fn new(request: impl Into<String>, gates: Gates) -> Self {
        Job {
            request: request.into(),
            stage: Stage::Requested,
            turn: Turn::Factory,
            spec: None,
            plan: Vec::new(),
            failed_rounds: 0,
            gates,
            answers: Vec::new(),
            fresh_sessions: true,
            review_rounds: 0,
            review: ReviewState::Pending,
            reviews: Vec::new(),
            review_head: None,
            skipped: Vec::new(),
            kind: JobKind::Request,
        }
    }

    /// A gap job found by job `source_job` after Renovate pull request
    /// `source_pr` merged. It waits at [`Stage::Proposed`] and holds `gates`
    /// for the run that follows an approval.
    #[must_use]
    pub fn proposal(
        request: impl Into<String>,
        gates: Gates,
        source_job: i64,
        source_pr: u64,
    ) -> Self {
        let mut job = Job::new(request, gates);
        job.stage = Stage::Proposed;
        job.turn = Turn::User(Ask::Proposal);
        job.kind = JobKind::Gap {
            source_job,
            source_pr,
        };
        job
    }

    /// A job that watches Renovate pull request `number`. Its request text is
    /// the pull request's title.
    #[must_use]
    pub fn renovate(number: u64, title: impl Into<String>, url: impl Into<String>) -> Self {
        let title = title.into();
        let mut job = Job::new(title.clone(), Gates::default());
        job.stage = Stage::Renovate;
        job.kind = JobKind::Renovate(Box::new(RenovatePr {
            number,
            title,
            url: url.into(),
            head: None,
            reruns: 0,
            auto_reruns: 0,
            verdict: None,
            stopped: None,
            comments: Vec::new(),
            runs: 0,
            gaps: None,
            pending: None,
            rerun: RerunKind::Auto,
        }));
        job
    }

    #[must_use]
    pub fn kind(&self) -> &JobKind {
        &self.kind
    }

    /// The Renovate pull request the job watches, or `None` for a request.
    #[must_use]
    pub fn renovate_pr(&self) -> Option<&RenovatePr> {
        match &self.kind {
            JobKind::Renovate(pr) => Some(&**pr),
            JobKind::Request | JobKind::Gap { .. } => None,
        }
    }

    /// The Renovate job and pull request number a gap job came from.
    #[must_use]
    pub fn gap_source(&self) -> Option<(i64, u64)> {
        match &self.kind {
            JobKind::Gap {
                source_job,
                source_pr,
            } => Some((*source_job, *source_pr)),
            _ => None,
        }
    }

    #[must_use]
    pub fn gates(&self) -> Gates {
        self.gates
    }

    #[must_use]
    pub fn request(&self) -> &str {
        &self.request
    }

    #[must_use]
    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    #[must_use]
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    #[must_use]
    pub fn answers(&self) -> &[Answered] {
        &self.answers
    }

    /// Whether the spec, plan and implement runs start a session with a
    /// self-contained prompt. `false` for a job created before that change.
    #[must_use]
    pub fn fresh_sessions(&self) -> bool {
        self.fresh_sessions
    }

    #[must_use]
    pub fn spec(&self) -> Option<&str> {
        self.spec.as_deref()
    }

    #[must_use]
    pub fn plan(&self) -> &[PlannedPr] {
        &self.plan
    }

    /// Whether the reviewer has approved the current head. The scheduler reads
    /// it to decide, once CI is green, whether to start a review or go on to
    /// the merge.
    #[must_use]
    pub fn review_state(&self) -> ReviewState {
        self.review
    }

    /// Whether the factory skipped planned pull request `pr` because its
    /// branch had nothing to publish.
    #[must_use]
    pub fn is_skipped(&self, pr: usize) -> bool {
        self.skipped.contains(&pr)
    }

    /// The review passes so far, oldest first.
    #[must_use]
    pub fn reviews(&self) -> &[ReviewPass] {
        &self.reviews
    }

    /// The command the factory runs next, or `None` while the job waits for
    /// the user or for capacity, or has ended.
    #[must_use]
    pub fn next_command(&self) -> Option<Command> {
        match (&self.turn, &self.stage) {
            (Turn::Agent(task), _) => Some(Command::RunAgent {
                session: self.session(),
                task: task.clone(),
            }),
            (Turn::Factory, Stage::Verifying { pr }) => Some(Command::RunChecks { pr: *pr }),
            (Turn::User(Ask::PublishApproval { summary: None }), Stage::Verifying { pr }) => {
                Some(Command::Summarize { pr: *pr })
            }
            (Turn::Factory, Stage::PrOpen { pr }) => Some(Command::Publish { pr: *pr }),
            (Turn::Factory, Stage::CiAndReview { pr }) => {
                self.plan[*pr].number.map(|number| Command::WatchPr {
                    pr: *pr,
                    number,
                    watch_only: false,
                })
            }
            // The review stopped and waits for the user. Keep watching the
            // pull request so the job moves on when the user merges it on
            // GitHub and fails when they close it, without merging or
            // restarting the reviewer on its own.
            (Turn::User(Ask::ReviewStopped { .. }), Stage::CiAndReview { pr }) => {
                self.plan[*pr].number.map(|number| Command::WatchPr {
                    pr: *pr,
                    number,
                    watch_only: true,
                })
            }
            (_, Stage::Renovate) => self.renovate_command(),
            (Turn::Factory, Stage::Packaging) => self.renovate_pr().map(|pr| match &pr.pending {
                Some(Pending::Propose { gaps, range }) => Command::ProposeGaps {
                    gaps: gaps.clone(),
                    range: range.clone(),
                },
                _ => Command::GatherPackaging { number: pr.number },
            }),
            (Turn::RerunCi { reason }, Stage::CiAndReview { pr }) => {
                self.plan[*pr].number.map(|number| Command::RerunCi {
                    pr: *pr,
                    number,
                    reason: reason.clone(),
                })
            }
            _ => None,
        }
    }

    fn renovate_command(&self) -> Option<Command> {
        let pr = self.renovate_pr()?;
        let number = pr.number;
        match &self.turn {
            Turn::Factory => Some(match &pr.pending {
                Some(Pending::Comment { body, .. }) => Command::PostComment {
                    number,
                    body: body.clone(),
                },
                Some(Pending::Merge) => Command::MergeRenovate {
                    number,
                    head: pr.head.clone()?,
                },
                None | Some(Pending::Propose { .. }) => Command::WatchRenovate {
                    number,
                    verdict_head: pr.verdict.as_ref().map(|verdict| verdict.head.clone()),
                    watch_only: false,
                },
            }),
            Turn::RerunCi { reason } => Some(Command::RerunRenovate {
                number,
                reason: reason.clone(),
            }),
            Turn::User(Ask::Renovate { state, .. }) => Some(Command::WatchRenovate {
                number,
                verdict_head: pr.verdict.as_ref().map(|verdict| verdict.head.clone()),
                watch_only: *state != RenovateWait::Ready,
            }),
            _ => None,
        }
    }

    /// Applies `event`. On `Err` the job is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`Rejected`] when the event does not apply to the job's stage
    /// and turn, or carries data that does not fit the job.
    pub fn apply(&mut self, event: Event) -> Result<(), Rejected> {
        let (stage, turn) = self.transition(event)?;
        if let Stage::Implementing { .. } = stage {
            self.failed_rounds = 0;
            self.review_rounds = 0;
        }
        self.stage = stage;
        self.turn = turn;
        Ok(())
    }

    fn transition(&mut self, event: Event) -> Result<(Stage, Turn), Rejected> {
        use Event as E;
        use Stage as S;

        if let E::Retry = event {
            return self.retry();
        }

        if let E::Dismiss = event {
            return self.dismiss();
        }

        if self.is_finished() {
            return Err(self.unexpected(event));
        }

        let next = match (&self.stage, &self.turn, event) {
            (_, _, E::Cancel) => (S::Cancelled, Turn::Done),

            (S::Requested, Turn::Factory, E::Start) => (
                S::Clarifying,
                Turn::Agent(Task::Clarify {
                    request: self.request.clone(),
                }),
            ),

            (S::Requested, Turn::Factory, E::Edit { request }) => {
                self.request = request;
                (S::Requested, Turn::Factory)
            }

            (stage, Turn::Agent(_), E::Agent(AgentResult::Questions(questions))) => {
                (stage.clone(), Turn::User(Ask::Answers { questions }))
            }
            (stage, Turn::Agent(task), E::AgentFailed { reason }) => (
                Self::failed(
                    reason,
                    Some(RetryTarget::Agent {
                        stage: Box::new(stage.clone()),
                        task: Box::new(task.clone()),
                    }),
                ),
                Turn::Done,
            ),

            (stage, turn, E::FactoryFailed { reason }) if self.interruptible(turn) => (
                Self::failed(reason, Some(RetryTarget::resume(stage, turn))),
                Turn::Done,
            ),

            (_, Turn::User(Ask::Answers { questions }), E::Answer(answers)) => {
                let questions = questions.clone();
                return self.record_answers(questions, answers);
            }

            (S::Clarifying, Turn::Agent(_), E::Agent(AgentResult::Clarified)) => {
                (S::Spec, Turn::Agent(Task::WriteSpec))
            }

            (S::Spec, Turn::Agent(_), E::Agent(AgentResult::Spec(spec))) => {
                self.spec = Some(spec);
                if self.gates.spec {
                    (S::Spec, Turn::User(Ask::SpecApproval))
                } else {
                    (S::Plan, Turn::Agent(Task::WritePlan))
                }
            }
            (S::Spec, Turn::User(Ask::SpecApproval), E::Approve) => {
                (S::Plan, Turn::Agent(Task::WritePlan))
            }

            (S::Plan, Turn::Agent(_), E::Agent(AgentResult::Plan(plan))) => {
                if plan.is_empty() {
                    return Err(Rejected::EmptyPlan);
                }
                self.plan = plan;
                if self.gates.plan {
                    (S::Plan, Turn::User(Ask::PlanApproval))
                } else {
                    self.implement(0)
                }
            }
            (S::Plan, Turn::User(Ask::PlanApproval), E::Approve) => self.implement(0),

            (
                stage @ (S::Spec | S::Plan),
                Turn::User(Ask::SpecApproval | Ask::PlanApproval),
                E::RequestChanges { feedback },
            ) => (stage.clone(), Turn::Agent(Task::Revise { feedback })),

            (
                S::CiAndReview { .. },
                _,
                event @ (E::StartReview { .. }
                | E::ReviewApproved
                | E::ReviewChanges { .. }
                | E::ReviewFailed { .. }),
            ) => return self.deliver_review(event),

            (S::Renovate, _, event) => return self.deliver_renovate(event),
            (S::Packaging, _, event) => return self.deliver_packaging(event),

            (S::Proposed, Turn::User(Ask::Proposal), E::Approve) => (S::Requested, Turn::Factory),

            (
                S::Implementing { .. }
                | S::Verifying { .. }
                | S::PrOpen { .. }
                | S::CiAndReview { .. },
                _,
                event,
            ) => {
                return self.deliver(event);
            }

            (_, _, event) => return Err(self.unexpected(event)),
        };
        Ok(next)
    }

    fn deliver(&mut self, event: Event) -> Result<(Stage, Turn), Rejected> {
        use Event as E;
        use Stage as S;

        let pr = self.current_pr();
        let next = match (&self.stage, &self.turn, event) {
            (
                S::Implementing { pr } | S::Verifying { pr } | S::CiAndReview { pr },
                Turn::Agent(_),
                E::Agent(AgentResult::CommitsReady(description)),
            ) => {
                let pr = *pr;
                self.plan[pr].description = Some(description);
                (S::Verifying { pr }, Turn::Factory)
            }

            (S::Verifying { pr }, Turn::Factory, E::ChecksPassed) => {
                if self.gates.publish && self.plan[*pr].number.is_none() {
                    (
                        S::Verifying { pr: *pr },
                        Turn::User(Ask::PublishApproval { summary: None }),
                    )
                } else {
                    (S::PrOpen { pr: *pr }, Turn::Factory)
                }
            }
            (
                S::Verifying { pr },
                Turn::User(Ask::PublishApproval { summary: None }),
                E::Summarized(summary),
            ) => (
                S::Verifying { pr: *pr },
                Turn::User(Ask::PublishApproval {
                    summary: Some(summary),
                }),
            ),
            (S::Verifying { pr }, Turn::User(Ask::PublishApproval { .. }), E::Approve) => {
                (S::PrOpen { pr: *pr }, Turn::Factory)
            }
            (
                S::Verifying { pr },
                Turn::User(Ask::PublishApproval { .. }),
                E::RequestChanges { feedback },
            ) => (
                S::Verifying { pr: *pr },
                Turn::Agent(Task::Revise { feedback }),
            ),
            (S::Verifying { pr }, Turn::Factory, E::ChecksFailed { report }) => {
                let pr = *pr;
                self.fail_round(S::Verifying { pr }, Turn::Agent(Task::FixChecks { report }))
            }

            (S::PrOpen { pr }, Turn::Factory, E::Published { number }) => {
                let pr = *pr;
                self.plan[pr].number = Some(number);
                self.review = ReviewState::Pending;
                (S::CiAndReview { pr }, Turn::Factory)
            }
            (S::PrOpen { pr }, Turn::Factory, E::NothingToPublish) => {
                let pr = *pr;
                self.skip(pr)
            }
            (S::CiAndReview { pr }, Turn::Factory, E::CiFailed { log }) => {
                self.fail_round(S::CiAndReview { pr: *pr }, Turn::Agent(Task::FixCi { log }))
            }
            (S::CiAndReview { pr }, Turn::Factory, E::Conflicted { base }) => self.fail_round(
                S::CiAndReview { pr: *pr },
                Turn::Agent(Task::Rebase { base }),
            ),
            (
                S::CiAndReview { pr },
                Turn::Agent(Task::FixCi { .. }),
                E::Agent(AgentResult::RerunCi { reason }),
            ) => {
                let pr = *pr;
                self.fail_round(S::CiAndReview { pr }, Turn::RerunCi { reason })
            }
            (S::CiAndReview { pr }, Turn::RerunCi { .. }, E::CiRerun) => {
                (S::CiAndReview { pr: *pr }, Turn::Factory)
            }
            (S::CiAndReview { pr }, Turn::Factory, E::ReviewComments { comments }) => (
                S::CiAndReview { pr: *pr },
                Turn::Agent(Task::AddressReview { comments }),
            ),

            (S::Verifying { .. } | S::PrOpen { .. } | S::CiAndReview { .. }, _, E::Merged)
                if pr.is_some_and(|pr| self.plan[pr].number.is_some()) =>
            {
                self.after(pr.unwrap_or(0))
            }
            (
                S::Verifying { .. } | S::PrOpen { .. } | S::CiAndReview { .. },
                _,
                E::ClosedUnmerged,
            ) if pr.is_some_and(|pr| self.plan[pr].number.is_some()) => (
                Self::failed("the pull request was closed without merging", None),
                Turn::Done,
            ),

            (_, _, event) => return Err(self.unexpected(event)),
        };
        Ok(next)
    }

    /// Handles the events of a Renovate job. The observation rules live in
    /// [`RenovatePr::decide`].
    fn deliver_renovate(&mut self, event: Event) -> Result<(Stage, Turn), Rejected> {
        use Event as E;

        let turn = self.turn.clone();
        let Some(pr) = self.renovate_mut() else {
            return Err(self.unexpected(event));
        };
        let next = match (&turn, event) {
            (
                Turn::Agent(Task::AnalyzeFailure { .. }),
                E::Agent(AgentResult::FailureVerdict { transient, reason }),
            ) => pr.on_failure_verdict(transient, &reason),
            (Turn::Agent(Task::CheckDiff { .. }), E::Agent(AgentResult::DiffVerdict(found))) => {
                pr.on_diff_verdict(*found)
            }
            (Turn::RerunCi { .. }, E::CiRerun) => {
                match pr.rerun {
                    RerunKind::Auto => {
                        pr.reruns += 1;
                        pr.auto_reruns += 1;
                    }
                    RerunKind::Analysed => pr.reruns += 1,
                    RerunKind::Retry => {}
                }
                Turn::Factory
            }
            (Turn::RerunCi { .. }, E::NothingToRerun) => pr.stop(
                CommentKind::NothingToRerun,
                "CI fails on a check that has no failed GitHub Actions run, so the factory cannot \
                 rerun it."
                    .to_owned(),
                "CI fails on a check that has no failed GitHub Actions run to rerun. The factory \
                 leaves this pull request to a person."
                    .to_owned(),
            ),
            (Turn::Factory, E::MergeFailed) => {
                if pr.pending.take() != Some(Pending::Merge) {
                    return Err(self.unexpected(E::MergeFailed));
                }
                Turn::Factory
            }
            (Turn::Factory, E::Commented) => {
                let Some(Pending::Comment { kind, then, .. }) = pr.pending.take() else {
                    return Err(self.unexpected(E::Commented));
                };
                if let Some(head) = pr.head.clone() {
                    pr.comments.push((head, kind));
                }
                Turn::User(then)
            }
            (
                Turn::Factory | Turn::User(Ask::Renovate { .. }),
                E::RenovateObserved {
                    head,
                    ci,
                    limits,
                    auto_merge,
                },
            ) => {
                if matches!(pr.pending, Some(Pending::Comment { .. })) {
                    return Err(self.unexpected(E::RenovateObserved {
                        head,
                        ci,
                        limits,
                        auto_merge,
                    }));
                }
                let waiting = matches!(
                    turn,
                    Turn::User(Ask::Renovate {
                        state: RenovateWait::Stopped | RenovateWait::Unsafe,
                        ..
                    })
                );
                if pr.head.as_deref() != Some(head.as_str()) {
                    pr.reset(head);
                } else if waiting {
                    return Ok((Stage::Renovate, turn));
                }
                pr.pending = None;
                pr.decide(ci, limits, auto_merge)
            }
            (_, E::Merged) => return Ok((Stage::Merged, Turn::Done)),
            (
                _,
                E::RenovateMerged {
                    packaging,
                    manifest_changed,
                },
            ) => {
                if !(packaging && manifest_changed) {
                    return Ok((Stage::Merged, Turn::Done));
                }
                pr.pending = None;
                return Ok((Stage::Packaging, Turn::Factory));
            }
            (_, E::ClosedUnmerged) => return Ok((Stage::Cancelled, Turn::Done)),
            (_, event) => return Err(self.unexpected(event)),
        };
        Ok((Stage::Renovate, next))
    }

    /// Handles the events of the packaging analysis of a merged Renovate
    /// pull request.
    fn deliver_packaging(&mut self, event: Event) -> Result<(Stage, Turn), Rejected> {
        use Event as E;

        let turn = self.turn.clone();
        let Some(pr) = self.renovate_mut() else {
            return Err(self.unexpected(event));
        };
        let next = match (&turn, event) {
            (Turn::Factory, E::PackagingGathered(context)) if pr.pending.is_none() => {
                pr.runs += 1;
                Turn::Agent(Task::FindGaps { context })
            }
            (Turn::Agent(Task::FindGaps { context }), E::Agent(AgentResult::Gaps(gaps))) => {
                let range = context.from.clone().zip(context.to.clone());
                pr.pending = Some(Pending::Propose { gaps, range });
                Turn::Factory
            }
            (
                Turn::Factory,
                E::RenovateMerged {
                    packaging,
                    manifest_changed,
                },
            ) if pr.pending.is_none() && !(packaging && manifest_changed) => {
                return Ok((Stage::Merged, Turn::Done));
            }
            (Turn::Factory, E::GapsProposed { jobs })
                if matches!(pr.pending, Some(Pending::Propose { .. })) =>
            {
                pr.pending = None;
                pr.gaps = Some(jobs);
                return Ok((Stage::Merged, Turn::Done));
            }
            (_, event) => return Err(self.unexpected(event)),
        };
        Ok((Stage::Packaging, next))
    }

    fn renovate_mut(&mut self) -> Option<&mut RenovatePr> {
        match &mut self.kind {
            JobKind::Renovate(pr) => Some(&mut **pr),
            JobKind::Request | JobKind::Gap { .. } => None,
        }
    }

    fn session(&self) -> Session {
        if let Some(pr) = self.renovate_pr() {
            return Session::Renovate(pr.runs);
        }
        if let (Stage::CiAndReview { pr }, Turn::Agent(Task::Review)) = (&self.stage, &self.turn) {
            return Session::Review(*pr);
        }
        match &self.stage {
            Stage::Implementing { pr }
            | Stage::Verifying { pr }
            | Stage::PrOpen { pr }
            | Stage::CiAndReview { pr } => Session::Pr(*pr),
            Stage::Spec if self.fresh_sessions => Session::Spec,
            Stage::Plan if self.fresh_sessions => Session::Plan,
            _ => Session::Planning,
        }
    }

    fn implement(&self, pr: usize) -> (Stage, Turn) {
        (
            Stage::Implementing { pr },
            Turn::Agent(Task::Implement {
                pr: self.plan[pr].clone(),
            }),
        )
    }

    fn skip(&mut self, pr: usize) -> (Stage, Turn) {
        match self.plan[pr].number {
            None => {
                self.skipped.push(pr);
                self.after(pr)
            }
            Some(number) => (
                Self::failed(
                    format!(
                        "the branch of pull request #{number} has no commits after the base \
                         branch, and pushing it would close the pull request"
                    ),
                    None,
                ),
                Turn::Done,
            ),
        }
    }

    fn after(&self, pr: usize) -> (Stage, Turn) {
        let next = pr + 1;
        if next < self.plan.len() {
            self.implement(next)
        } else {
            (Stage::Merged, Turn::Done)
        }
    }

    fn fail_round(&mut self, stage: Stage, next: Turn) -> (Stage, Turn) {
        self.failed_rounds += 1;
        if self.failed_rounds >= MAX_FAILED_ROUNDS {
            let reason =
                format!("{MAX_FAILED_ROUNDS} failed check or CI rounds on one pull request");
            let retry = match &stage {
                Stage::Verifying { pr } => Some(RetryTarget::Verifying { pr: *pr }),
                Stage::CiAndReview { pr } => Some(RetryTarget::CiAndReview { pr: *pr }),
                _ => None,
            };
            (Self::failed(reason, retry), Turn::Done)
        } else {
            (stage, next)
        }
    }

    /// Handles the LLM review events in `ci-and-review`. The review starts
    /// once CI is green, records each pass, and loops a changes verdict back
    /// to the implement session until the review cap, where it stops for the
    /// user. A reviewer failure stops it the same way, so the job never merges
    /// without an approval.
    fn deliver_review(&mut self, event: Event) -> Result<(Stage, Turn), Rejected> {
        use Event as E;
        use Stage as S;

        let next = match (&self.stage, &self.turn, event) {
            (S::CiAndReview { pr }, Turn::Factory, E::StartReview { head })
                if self.review == ReviewState::Pending =>
            {
                let pr = *pr;
                if self.review_unchanged(pr, &head) {
                    (
                        S::CiAndReview { pr },
                        Turn::User(Ask::ReviewStopped {
                            reason: UNCHANGED_REASON.to_owned(),
                            stop: ReviewStop::Unchanged,
                        }),
                    )
                } else {
                    self.review_head = Some(head);
                    (S::CiAndReview { pr }, Turn::Agent(Task::Review))
                }
            }
            (S::CiAndReview { pr }, Turn::Agent(Task::Review), E::ReviewApproved) => {
                let pr = *pr;
                self.record_review(pr, Verdict::Approve, None);
                self.review = ReviewState::Approved;
                (S::CiAndReview { pr }, Turn::Factory)
            }
            (S::CiAndReview { pr }, Turn::Agent(Task::Review), E::ReviewChanges { feedback }) => {
                let pr = *pr;
                self.record_review(pr, Verdict::Changes, Some(feedback.clone()));
                self.review_rounds += 1;
                if self.review_rounds >= MAX_REVIEW_ROUNDS {
                    let reason = format!("{MAX_REVIEW_ROUNDS} review rounds on one pull request");
                    (
                        S::CiAndReview { pr },
                        Turn::User(Ask::ReviewStopped {
                            reason,
                            stop: ReviewStop::CapReached { feedback },
                        }),
                    )
                } else {
                    (
                        S::CiAndReview { pr },
                        Turn::Agent(Task::AddressReview { comments: feedback }),
                    )
                }
            }
            (S::CiAndReview { pr }, Turn::Agent(Task::Review), E::ReviewFailed { reason }) => (
                S::CiAndReview { pr: *pr },
                Turn::User(Ask::ReviewStopped {
                    reason,
                    stop: ReviewStop::ReviewerFailed,
                }),
            ),
            (_, _, event) => return Err(self.unexpected(event)),
        };
        Ok(next)
    }

    /// Records the answers to the agent's questions and resumes its session.
    fn record_answers(
        &mut self,
        questions: Vec<Question>,
        answers: Vec<String>,
    ) -> Result<(Stage, Turn), Rejected> {
        if answers.len() != questions.len() {
            return Err(Rejected::AnswerCount {
                expected: questions.len(),
                got: answers.len(),
            });
        }
        self.answers
            .extend(questions.iter().zip(&answers).map(|(q, a)| Answered {
                question: q.text.clone(),
                answer: a.clone(),
            }));
        Ok((
            self.stage.clone(),
            Turn::Agent(Task::Answer { questions, answers }),
        ))
    }

    fn record_review(&mut self, pr: usize, verdict: Verdict, feedback: Option<String>) {
        let number = u32::try_from(self.reviews.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        self.reviews.push(ReviewPass {
            number,
            verdict,
            feedback,
            pr: Some(pr),
            head: self.review_head.clone(),
            answers_seen: self.answers.len(),
        });
    }

    /// Whether a review of `head` on pull request `pr` would read what the
    /// last pass read: that pass asked for changes on the same head, and the
    /// requester has answered nothing since.
    fn review_unchanged(&self, pr: usize, head: &str) -> bool {
        self.reviews_of(pr).last().is_some_and(|pass| {
            pass.verdict == Verdict::Changes
                && pass.head.as_deref() == Some(head)
                && pass.answers_seen == self.answers.len()
        })
    }

    /// The review passes on pull request `pr`, in order. Passes stored before
    /// [`ReviewPass::pr`] existed belong to no pull request.
    pub fn reviews_of(&self, pr: usize) -> impl Iterator<Item = &ReviewPass> {
        self.reviews.iter().filter(move |pass| pass.pr == Some(pr))
    }

    fn current_pr(&self) -> Option<usize> {
        match self.stage {
            Stage::Implementing { pr }
            | Stage::Verifying { pr }
            | Stage::PrOpen { pr }
            | Stage::CiAndReview { pr } => Some(pr),
            _ => None,
        }
    }

    /// Applies [`Event::Retry`]. A stopped review resumes by its case; every
    /// other job defers to [`Job::retry_step`]. On `Err` the job is unchanged.
    fn retry(&mut self) -> Result<(Stage, Turn), Rejected> {
        if let Turn::User(Ask::Renovate {
            state: RenovateWait::Stopped,
            ..
        }) = &self.turn
            && let Some(pr) = self.renovate_mut()
        {
            pr.reruns = 0;
            pr.auto_reruns = 0;
            pr.stopped = None;
            pr.rerun = RerunKind::Retry;
            return Ok((
                Stage::Renovate,
                Turn::RerunCi {
                    reason: "retry requested by the user".to_owned(),
                },
            ));
        }

        if let (Stage::CiAndReview { pr }, Turn::User(Ask::ReviewStopped { stop, .. })) =
            (&self.stage, &self.turn)
        {
            let pr = *pr;
            if self.plan[pr].number.is_none() {
                return Err(self.unexpected(Event::Retry));
            }
            return Ok(match stop.clone() {
                // A reviewer run failed, so rerun the reviewer in the same
                // review session. The round counts stay as they are.
                ReviewStop::ReviewerFailed | ReviewStop::Unchanged => {
                    (Stage::CiAndReview { pr }, Turn::Agent(Task::Review))
                }
                // The review hit its cap, so hand the last feedback back to
                // the implement session and give the pull request five fresh
                // review rounds.
                ReviewStop::CapReached { feedback } => {
                    self.review_rounds = 0;
                    (
                        Stage::CiAndReview { pr },
                        Turn::Agent(Task::AddressReview { comments: feedback }),
                    )
                }
            });
        }

        match self.retry_step() {
            Some((stage, turn)) => {
                let resumed = matches!(
                    &self.stage,
                    Stage::Failed {
                        retry: Some(RetryTarget::Resume { .. }),
                        ..
                    }
                );
                if !matches!(turn, Turn::Agent(_)) && !resumed {
                    self.failed_rounds = 0;
                }
                Ok((stage, turn))
            }
            None => Err(self.unexpected(Event::Retry)),
        }
    }

    fn failed(reason: impl Into<String>, retry: Option<RetryTarget>) -> Stage {
        Stage::Failed {
            reason: reason.into(),
            retry,
            dismissed: false,
        }
    }

    fn dismiss(&mut self) -> Result<(Stage, Turn), Rejected> {
        match &self.stage {
            Stage::Proposed => Ok((Stage::Dismissed, Turn::Done)),
            Stage::Failed {
                reason,
                retry,
                dismissed: false,
            } => Ok((
                Stage::Failed {
                    reason: reason.clone(),
                    retry: retry.clone(),
                    dismissed: true,
                },
                self.turn.clone(),
            )),
            _ => Err(self.unexpected(Event::Dismiss)),
        }
    }

    /// Whether a failed or proposed job was dismissed from the board. False
    /// for every other stage.
    #[must_use]
    pub fn is_dismissed(&self) -> bool {
        matches!(
            self.stage,
            Stage::Dismissed
                | Stage::Failed {
                    dismissed: true,
                    ..
                }
        )
    }

    /// Whether a [`Event::Retry`] would resume the job. True for a stopped
    /// review on a pull request that has a number, and for a job that failed
    /// because it used up its failed rounds or an agent run failed: one with a
    /// retry target, or a legacy job recognised by its reason that has a pull
    /// request number. A stopped Renovate job can retry too. False for every
    /// other failure or user ask, and for a legacy job with no pull request
    /// number.
    #[must_use]
    pub fn can_retry(&self) -> bool {
        if let Turn::User(Ask::Renovate { state, .. }) = &self.turn {
            return *state == RenovateWait::Stopped;
        }
        if let (Stage::CiAndReview { pr }, Turn::User(Ask::ReviewStopped { .. })) =
            (&self.stage, &self.turn)
        {
            return self.plan[*pr].number.is_some();
        }
        self.retry_step().is_some()
    }

    /// The stage and turn a retry resumes at, or `None` when the job cannot
    /// be retried.
    fn retry_step(&self) -> Option<(Stage, Turn)> {
        let Stage::Failed { reason, retry, .. } = &self.stage else {
            return None;
        };
        match retry {
            Some(RetryTarget::Verifying { pr }) => {
                Some((Stage::Verifying { pr: *pr }, Turn::Factory))
            }
            Some(RetryTarget::CiAndReview { pr }) => {
                Some((Stage::CiAndReview { pr: *pr }, Turn::Factory))
            }
            Some(RetryTarget::Resume { stage, turn }) => {
                Some(((**stage).clone(), (**turn).clone()))
            }
            Some(RetryTarget::Agent { stage, task }) => {
                Some(((**stage).clone(), Turn::Agent((**task).clone())))
            }
            // A legacy job did not record a target or the stage it failed
            // in. Resume at the highest-numbered pull request: watching it
            // either finds work for the agent or, when it was merged, moves
            // on to the next one.
            None if reason == LEGACY_ROUNDS_REASON || reason.starts_with(LEGACY_AGENT_REASON) => {
                self.plan
                    .iter()
                    .enumerate()
                    .filter(|(_, pr)| pr.number.is_some())
                    .max_by_key(|(_, pr)| pr.number)
                    .map(|(pr, _)| (Stage::CiAndReview { pr }, Turn::Factory))
            }
            None => None,
        }
    }

    fn interruptible(&self, turn: &Turn) -> bool {
        !matches!(turn, Turn::Agent(_)) && !self.is_finished()
    }

    /// Whether the job is merged, failed or cancelled, so no step is left.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(
            self.stage,
            Stage::Merged | Stage::Cancelled | Stage::Dismissed | Stage::Failed { .. }
        )
    }

    fn unexpected(&self, event: Event) -> Rejected {
        Rejected::Unexpected {
            stage: self.stage.clone(),
            event,
        }
    }
}
