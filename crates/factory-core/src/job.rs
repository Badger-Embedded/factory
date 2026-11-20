use std::fmt;

use serde::{Deserialize, Serialize};

/// Check failures, CI failures and merge conflicts a job may hit on one
/// pull request before it fails. A conflict counts, so an agent that cannot
/// resolve one does not rebase forever.
pub const MAX_FAILED_ROUNDS: u32 = 5;

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
    Implementing { pr: usize },
    Verifying { pr: usize },
    PrOpen { pr: usize },
    CiAndReview { pr: usize },
    Merged,
    Cancelled,
    Failed { reason: String },
}

/// Who the job is waiting for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Turn {
    Agent(Task),
    Factory,
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
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentResult {
    Questions(Vec<Question>),
    Clarified,
    Spec(String),
    Plan(Vec<PlannedPr>),
    CommitsReady(PrDescription),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Start,
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
    /// The pull request no longer merges cleanly into `base`.
    Conflicted {
        base: String,
    },
    ReviewComments {
        comments: String,
    },
    Merged,
    ClosedUnmerged,
    Cancel,
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
            (Turn::Factory, Stage::CiAndReview { pr }) => self.plan[*pr]
                .number
                .map(|number| Command::WatchPr { pr: *pr, number }),
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
        }
        self.stage = stage;
        self.turn = turn;
        Ok(())
    }

    fn transition(&mut self, event: Event) -> Result<(Stage, Turn), Rejected> {
        use Event as E;
        use Stage as S;

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

            (stage, Turn::Agent(_), E::Agent(AgentResult::Questions(questions))) => {
                (stage.clone(), Turn::User(Ask::Answers { questions }))
            }
            (_, Turn::Agent(_), E::AgentFailed { reason }) => (S::Failed { reason }, Turn::Done),

            (stage, Turn::User(Ask::Answers { questions }), E::Answer(answers)) => {
                if answers.len() != questions.len() {
                    return Err(Rejected::AnswerCount {
                        expected: questions.len(),
                        got: answers.len(),
                    });
                }
                let questions = questions.clone();
                self.answers
                    .extend(questions.iter().zip(&answers).map(|(q, a)| Answered {
                        question: q.text.clone(),
                        answer: a.clone(),
                    }));
                (
                    stage.clone(),
                    Turn::Agent(Task::Answer { questions, answers }),
                )
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
                self.fail_round(S::Verifying { pr }, Task::FixChecks { report })
            }

            (S::PrOpen { pr }, Turn::Factory, E::Published { number }) => {
                let pr = *pr;
                self.plan[pr].number = Some(number);
                (S::CiAndReview { pr }, Turn::Factory)
            }

            (S::CiAndReview { pr }, Turn::Factory, E::CiFailed { log }) => {
                let pr = *pr;
                self.fail_round(S::CiAndReview { pr }, Task::FixCi { log })
            }
            (S::CiAndReview { pr }, Turn::Factory, E::Conflicted { base }) => {
                let pr = *pr;
                self.fail_round(S::CiAndReview { pr }, Task::Rebase { base })
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
                S::Failed {
                    reason: "the pull request was closed without merging".into(),
                },
                Turn::Done,
            ),

            (_, _, event) => return Err(self.unexpected(event)),
        };
        Ok(next)
    }

    fn session(&self) -> Session {
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

    fn fail_round(&mut self, stage: Stage, fix: Task) -> (Stage, Turn) {
        self.failed_rounds += 1;
        if self.failed_rounds >= MAX_FAILED_ROUNDS {
            let reason =
                format!("{MAX_FAILED_ROUNDS} failed check or CI rounds on one pull request");
            (Stage::Failed { reason }, Turn::Done)
        } else {
            (stage, Turn::Agent(fix))
        }
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
