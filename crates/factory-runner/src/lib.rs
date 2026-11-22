//! Runs one agent run in a podman worker and turns its output into a
//! `factory_core` event.
//!
//! [`worker`] owns the generic `podman run` flags and the run loop; everything
//! agent-specific sits behind the [`AgentBackend`] interface, with Claude Code
//! as its first implementation in [`claude`].

mod agent;
mod checks;
mod claude;
mod codex;
mod copilot;
mod files;
mod git;
mod github;
mod images;
mod outcome;
mod prompt;
mod stream;
mod worker;

pub use agent::{AgentBackend, AgentItem, Credential, RunResult, TokenUsage, backend};
pub use checks::{checks_args, run_checks};
pub use factory_core::BranchSummary;
pub use files::{PlacedFile, place_files, show_files};
pub use git::Git;
pub use github::{
    Ci, Comment, FailedCheck, GitHub, Observation, PrDetails, PrState, PrStatus, RenovatePr, Rerun,
    actions_job_id, is_renovate_login, observe, parse_renovate_prs, parse_status,
};
pub use images::{place_images, show_images};
pub use outcome::{Expect, event_from_output, event_from_result, failed_event};
pub use prompt::{job_prompt, prompt};
pub use stream::{LogLine, McpServerStatus, ResultMessage, StreamItem, parse_line};
pub use worker::{
    McpConfig, McpUnavailable, RunReport, SessionRun, WorkerConfig, WorkerRun, podman_args,
    remove_container, run,
};
