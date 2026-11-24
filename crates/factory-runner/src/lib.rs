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
mod forge;
mod forgejo;
mod git;
mod github;
mod gitlab;
mod images;
mod outcome;
mod packages;
mod prompt;
mod stream;
mod worker;

pub use agent::{AgentBackend, AgentItem, Credential, RunResult, TokenUsage, backend};
pub use checks::{checks_args, run_checks};
pub use factory_core::BranchSummary;
pub use files::{PlacedFile, place_files, show_files};
pub use forge::{
    Access, Account, BoxFuture, Forge, ForgeAccount, ForgeKind, LookupError, NOT_PERMITTED,
    REJECTED, rejected_by_forge,
};
pub use forgejo::{Forgejo, ForgejoVersion, client as forgejo_client, version as forgejo_version};
pub use git::{Git, Remote, RemoteSource};
pub use github::{
    Ci, Comment, FailedCheck, GitHub, HeadCommit, Observation, PrDetails, PrInfo, PrState,
    PrStatus, RebaseCheckbox, RenovatePr, Rerun, actions_job_id, is_renovate_login, observe,
    parse_renovate_prs, parse_status, tick_rebase_checkbox,
};
pub use gitlab::{
    GitLab, encode as gitlab_encode, gitlab_job_id, is_not_found as gitlab_not_found,
    project as gitlab_project,
};
pub use images::{place_images, show_images};
pub use outcome::{Expect, event_from_output, event_from_result, failed_event};
pub use prompt::{job_prompt, prompt};
pub use stream::{LogLine, McpServerStatus, ResultMessage, StreamItem, parse_line};
pub use worker::{
    McpConfig, McpProxy, McpServerEntry, McpUnavailable, RunReport, STDIO_STARTUP_TIMEOUT_SECS,
    SessionRun, WorkerConfig, WorkerRun, podman_args, remove_container, run,
};
