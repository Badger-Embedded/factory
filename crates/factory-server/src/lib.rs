//! The factory's dashboard and job store.

mod check;
mod codex;
mod cost;
mod device_login;
mod github;
mod localtime;
mod mcp;
mod pages;
mod podman;
mod projects;
mod proxy;
mod registry;
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
pub use codex::{CodexLogin, CodexRun};
pub use cost::{codex_cost, format_cost, merge_fraction};
pub use device_login::{DeviceLogin, LoginProcess, LoginSpawner, LoginView, PodmanLogin};
pub use github::{
    BranchInfo, GhRepos, GitHubRepos, Permission, Protection, RepoChecker, RepoInfo, RepoReport,
    check_repo,
};
pub use mcp::{McpError, McpProjectSettings, McpServer, McpStages, McpTool, McpToolState};
pub use podman::PodmanExecutor;
pub use projects::{
    ImportError, Project, Projects, StageAgents, StageEfforts, StageModels, import_tomls,
};
pub use proxy::{
    ProxyEvent, ProxyEventFn, RunTokens, StoreUpstreams, Upstream, Upstreams, proxy_app,
};
pub use registry::{AgentEntry, AgentRegistry};
pub use scheduler::{
    AgentRequest, ChecksRequest, Executor, FINISHED_JOB_DAYS, LogSink, McpRuntime, PublishRequest,
    Published, ReadyAction, RerunRequest, Scheduler, SessionSink, SummaryRequest, WatchReport,
    WatchRequest, branch_name, ready_action, watch_only_event,
};
pub use secrets::{
    ClaudeToken, CodexAuthError, LOCAL_BACKEND, LocalSecrets, MASTER_KEY_LEN, SecretError,
    SecretRef, SecretStore, UnavailableSecrets, master_key_from_env, validate_codex_auth,
};
pub use settings::Settings;
pub use store::{
    CodexPrice, Collaborator, JobEvent, JobFile, JobRow, LogEntry, ProjectTotals, Store, StoreError,
};
pub use usage::PlanUsageCache;
pub use warnings::McpWarnings;
pub use web::{MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS, USER_HEADER, app};
pub use workers::{Slot, WorkerSlots};
