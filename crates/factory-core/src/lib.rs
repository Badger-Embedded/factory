//! The factory's job state machine.
//!
//! A [`Job`] changes only through [`Job::apply`]. What the factory has to do
//! next is a function of the job alone, [`Job::next_command`], so after a
//! crash the factory reads the job back and runs the same command again.
//! Every [`Command`] must therefore be safe to run twice.

mod agent;
mod job;
mod names;
mod usage;

pub use agent::{Agent, DEFAULT_AGENT, UnknownAgent};
pub use job::{
    AgentResult, Answered, Ask, BranchSummary, Command, Event, Gates, Job, MAX_FAILED_ROUNDS,
    MAX_REVIEW_ROUNDS, PlannedPr, PrDescription, Question, Rejected, RetryTarget, ReviewPass,
    ReviewState, ReviewStop, Session, Stage, Task, Turn, Verdict,
};
pub use usage::{ParseError, PlanUsage, PlanUsageSnapshot, UsageWindow};
