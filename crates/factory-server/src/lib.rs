//! The factory's dashboard and job store.

mod check;
mod cost;
mod github;
mod localtime;
mod mcp;
mod pages;
mod podman;
mod projects;
mod proxy;
mod scheduler;
mod secrets;
mod settings;
mod store;
mod timeline;
mod usage;
mod warnings;
mod web;
mod workers;

pub use check::{CheckOutcome, HttpMcpChecker, McpChecker, TOTAL_TIMEOUT, ToolList};
pub use cost::{format_cost, merge_fraction};
pub use github::{
    BranchInfo, GhRepos, GitHubRepos, Permission, Protection, RepoChecker, RepoInfo, RepoReport,
    check_repo,
};
pub use mcp::{McpError, McpProjectSettings, McpServer, McpStages, McpTool, McpToolState};
pub use podman::PodmanExecutor;
pub use projects::{ImportError, Project, Projects, StageEfforts, StageModels, import_tomls};
pub use proxy::{ProxyWarning, RunTokens, StoreUpstreams, Upstream, Upstreams, WarnFn, proxy_app};
pub use scheduler::{
    AgentRequest, ChecksRequest, Executor, FINISHED_JOB_DAYS, LogSink, McpRuntime, PublishRequest,
    Published, ReadyAction, RerunRequest, Scheduler, SummaryRequest, WatchReport, WatchRequest,
    branch_name, ready_action, watch_only_event,
};
pub use secrets::{
    ClaudeToken, LOCAL_BACKEND, LocalSecrets, MASTER_KEY_LEN, SecretError, SecretRef, SecretStore,
    UnavailableSecrets, master_key_from_env,
};
pub use settings::Settings;
pub use store::{JobEvent, JobFile, JobRow, LogEntry, ProjectTotals, Store, StoreError};
pub use usage::{
    CurlUsageSource, PLAN_USAGE_REFRESH, PlanUsageCache, UsageSource, refresh_forever, refresh_once,
};
pub use warnings::McpWarnings;
pub use web::{MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS, USER_HEADER, app};
pub use workers::{Slot, WorkerSlots};
