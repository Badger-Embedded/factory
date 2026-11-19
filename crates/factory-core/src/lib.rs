//! The factory's job state machine.
//!
//! A [`Job`] changes only through [`Job::apply`]. What the factory has to do
//! next is a function of the job alone, [`Job::next_command`], so after a
//! crash the factory reads the job back and runs the same command again.
//! Every [`Command`] must therefore be safe to run twice.

mod job;

pub use job::{
    AgentResult, Ask, Command, Event, Job, MAX_FAILED_ROUNDS, PlannedPr, Question, Rejected,
    Session, Stage, Task, Turn,
};
