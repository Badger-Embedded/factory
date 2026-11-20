//! Runs one Claude Code agent run in a podman worker and turns its output
//! into a `factory_core` event.

mod checks;
mod git;
mod github;
mod images;
mod outcome;
mod prompt;
mod stream;
mod worker;

pub use checks::{checks_args, run_checks};
pub use git::{BranchSummary, Git};
pub use github::{
    Ci, Comment, FailedCheck, GitHub, Observation, PrState, PrStatus, actions_job_id, observe,
    parse_status,
};
pub use images::{place_images, show_images};
pub use outcome::{Expect, event_from_result};
pub use prompt::prompt;
pub use stream::{LogLine, ResultMessage, StreamItem, parse_line};
pub use worker::{
    AGENT_SETTINGS, RunReport, SessionRun, WorkerConfig, WorkerRun, podman_args, remove_container,
    run,
};
