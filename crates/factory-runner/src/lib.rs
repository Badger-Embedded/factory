//! Runs one Claude Code agent run in a podman worker and turns its output
//! into a `factory_core` event.

mod git;
mod outcome;
mod prompt;
mod stream;
mod worker;

pub use git::Git;
pub use outcome::{Expect, event_from_result};
pub use prompt::prompt;
pub use stream::{LogLine, ResultMessage, StreamItem, parse_line};
pub use worker::{
    RunReport, SessionRun, WorkerConfig, WorkerRun, podman_args, remove_container, run,
};
