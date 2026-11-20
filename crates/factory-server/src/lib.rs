//! The factory's dashboard and job store.

mod cost;
mod github;
mod mcp;
mod pages;
mod podman;
mod projects;
mod scheduler;
mod secrets;
mod settings;
mod store;
mod timeline;
mod usage;
mod web;
mod workers;

pub use cost::{format_cost, merge_fraction};
pub use github::{
    BranchInfo, GhRepos, GitHubRepos, Permission, Protection, RepoChecker, RepoInfo, RepoReport,
    check_repo,
};
pub use mcp::{McpError, McpProjectSettings, McpServer, McpStages};
pub use podman::PodmanExecutor;
pub use projects::{ImportError, Project, Projects, StageEfforts, StageModels, import_tomls};
pub use scheduler::{
    AgentRequest, ChecksRequest, Executor, FINISHED_JOB_DAYS, LogSink, PublishRequest, Published,
    ReadyAction, RerunRequest, Scheduler, SummaryRequest, WatchReport, WatchRequest, branch_name,
    ready_action,
};
pub use secrets::{
    LOCAL_BACKEND, LocalSecrets, MASTER_KEY_LEN, SecretError, SecretRef, SecretStore,
    UnavailableSecrets, master_key_from_env,
};
pub use settings::Settings;
pub use store::{JobEvent, JobRow, LogEntry, ProjectTotals, Store, StoreError};
pub use usage::{
    CurlUsageSource, PLAN_USAGE_REFRESH, PlanUsageCache, UsageSource, refresh_forever, refresh_once,
};
pub use web::{MAX_IMAGE_BYTES, MAX_IMAGES, USER_HEADER, app};
pub use workers::{Slot, WorkerSlots};
