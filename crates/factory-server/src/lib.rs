//! The factory's dashboard and job store.

mod cost;
mod github;
mod pages;
mod podman;
mod projects;
mod scheduler;
mod settings;
mod store;
mod timeline;
mod usage;
mod web;

pub use cost::{format_cost, merge_fraction};
pub use github::{
    BranchInfo, GhRepos, GitHubRepos, Permission, RepoChecker, RepoInfo, RepoReport, check_repo,
};
pub use podman::PodmanExecutor;
pub use projects::{ImportError, Project, Projects, StageModels, import_tomls};
pub use scheduler::{
    AgentRequest, ChecksRequest, Executor, FINISHED_JOB_DAYS, LogSink, PublishRequest, Published,
    ReadyAction, RerunRequest, Scheduler, SummaryRequest, WatchReport, WatchRequest, branch_name,
    ready_action,
};
pub use settings::Settings;
pub use store::{JobEvent, JobRow, LogEntry, ProjectTotals, Store, StoreError};
pub use usage::{
    CurlUsageSource, PLAN_USAGE_REFRESH, PlanUsageCache, UsageSource, refresh_forever, refresh_once,
};
pub use web::{MAX_IMAGE_BYTES, MAX_IMAGES, USER_HEADER, app};
