use std::fmt;

/// Check or CI failures a job may hit on one pull request before it fails.
pub const MAX_FAILED_ROUNDS: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    request: String,
    stage: Stage,
    turn: Turn,
    spec: Option<String>,
    plan: Vec<PlannedPr>,
    failed_rounds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    Agent(Task),
    Factory,
    User(Ask),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Answers { questions: Vec<Question> },
    SpecApproval,
    PlanApproval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPr {
    pub title: String,
    pub summary: String,
    /// The GitHub pull request number, once the factory has opened it.
    pub number: Option<u64>,
}

/// The input for one agent run.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// The agent session a run continues. Clarifying, spec and plan share one
/// session, and each planned pull request gets its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Session {
    Planning,
    Pr(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    RunAgent {
        session: Session,
        task: Task,
    },
    RunChecks {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentResult {
    Questions(Vec<Question>),
    Clarified,
    Spec(String),
    Plan(Vec<PlannedPr>),
    CommitsReady,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Start,
    Agent(AgentResult),
    AgentFailed { reason: String },
    Answer(Vec<String>),
    Approve,
    RequestChanges { feedback: String },
    ChecksPassed,
    ChecksFailed { report: String },
    Published { number: u64 },
    CiFailed { log: String },
    ReviewComments { comments: String },
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
    pub fn new(request: impl Into<String>) -> Self {
        Job {
            request: request.into(),
            stage: Stage::Requested,
            turn: Turn::Factory,
            spec: None,
            plan: Vec::new(),
            failed_rounds: 0,
        }
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
            (Turn::Agent(task), stage) => Some(Command::RunAgent {
                session: session_of(stage),
                task: task.clone(),
            }),
            (Turn::Factory, Stage::Verifying { pr }) => Some(Command::RunChecks { pr: *pr }),
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
                (
                    stage.clone(),
                    Turn::Agent(Task::Answer {
                        questions: questions.clone(),
                        answers,
                    }),
                )
            }

            (S::Clarifying, Turn::Agent(_), E::Agent(AgentResult::Clarified)) => {
                (S::Spec, Turn::Agent(Task::WriteSpec))
            }

            (S::Spec, Turn::Agent(_), E::Agent(AgentResult::Spec(spec))) => {
                self.spec = Some(spec);
                (S::Spec, Turn::User(Ask::SpecApproval))
            }
            (S::Spec, Turn::User(Ask::SpecApproval), E::Approve) => {
                (S::Plan, Turn::Agent(Task::WritePlan))
            }

            (S::Plan, Turn::Agent(_), E::Agent(AgentResult::Plan(plan))) => {
                if plan.is_empty() {
                    return Err(Rejected::EmptyPlan);
                }
                self.plan = plan;
                (S::Plan, Turn::User(Ask::PlanApproval))
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
                E::Agent(AgentResult::CommitsReady),
            ) => (S::Verifying { pr: *pr }, Turn::Factory),

            (S::Verifying { pr }, Turn::Factory, E::ChecksPassed) => {
                (S::PrOpen { pr: *pr }, Turn::Factory)
            }
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

    fn is_finished(&self) -> bool {
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

fn session_of(stage: &Stage) -> Session {
    match stage {
        Stage::Implementing { pr }
        | Stage::Verifying { pr }
        | Stage::PrOpen { pr }
        | Stage::CiAndReview { pr } => Session::Pr(*pr),
        _ => Session::Planning,
    }
}
