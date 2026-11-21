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
    /// Run `task` again in `stage`, the stage the failed run belonged to.
    /// The stage picks the session, so the run resumes the session that
    /// failed, in the same checkout.
    Agent { stage: Box<Stage>, task: Box<Task> },
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
    /// The pull request no longer merges cleanly into `base`.
    Conflicted {
        base: String,
    },
    ReviewComments {
        comments: String,
    },
    /// CI is green and the project has LLM review on, so the reviewer should
    /// run before the factory merges.
    StartReview,
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
    Merged,
    ClosedUnmerged,
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
                event @ (E::StartReview
                | E::ReviewApproved
                | E::ReviewChanges { .. }
                | E::ReviewFailed { .. }),
            ) => return self.deliver_review(event),

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
                let next = pr.map_or(0, |pr| pr + 1);
                if next < self.plan.len() {
                    self.implement(next)
                } else {
                    (S::Merged, Turn::Done)
                }
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

    fn session(&self) -> Session {
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
            (S::CiAndReview { pr }, Turn::Factory, E::StartReview)
                if self.review == ReviewState::Pending =>
            {
                (S::CiAndReview { pr: *pr }, Turn::Agent(Task::Review))
            }
            (S::CiAndReview { pr }, Turn::Agent(Task::Review), E::ReviewApproved) => {
                let pr = *pr;
                self.record_review(Verdict::Approve, None);
                self.review = ReviewState::Approved;
                (S::CiAndReview { pr }, Turn::Factory)
            }
            (S::CiAndReview { pr }, Turn::Agent(Task::Review), E::ReviewChanges { feedback }) => {
                let pr = *pr;
                self.record_review(Verdict::Changes, Some(feedback.clone()));
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

    fn record_review(&mut self, verdict: Verdict, feedback: Option<String>) {
        let number = u32::try_from(self.reviews.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        self.reviews.push(ReviewPass {
            number,
            verdict,
            feedback,
        });
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
                ReviewStop::ReviewerFailed => {
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
                if !matches!(turn, Turn::Agent(_)) {
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

    /// Whether a failed job was dismissed from the board. False for every
    /// other stage.
    #[must_use]
    pub fn is_dismissed(&self) -> bool {
        matches!(
            self.stage,
            Stage::Failed {
                dismissed: true,
                ..
            }
        )
    }

    /// Whether a [`Event::Retry`] would resume the job. True for a stopped
    /// review on a pull request that has a number, and for a job that failed
    /// because it used up its failed rounds or an agent run failed: one with a
    /// retry target, or a legacy job recognised by its reason that has a pull
    /// request number. False for every other failure or user ask, and for a
    /// legacy job with no pull request number.
    #[must_use]
    pub fn can_retry(&self) -> bool {
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

    /// Whether the job is merged, failed or cancelled, so no step is left.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(
            self.stage,
            Stage::Merged | Stage::Cancelled | Stage::Failed { .. }
        )
    }

    fn unexpected(&self, event: Event) -> Rejected {
        Rejected::Unexpected {
            stage: self.stage.clone(),
            event,
        }
    }
}
