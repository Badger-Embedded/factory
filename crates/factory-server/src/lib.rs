//! The factory's dashboard and job store.

mod accounts;
mod check;
mod codex;
mod copilot;
mod cost;
pub mod csrf;
mod device_login;
pub mod forge_tokens;
mod forges;
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

pub use accounts::{Accounts, Resolved};
pub use check::{CheckOutcome, HttpMcpChecker, McpChecker, TOTAL_TIMEOUT, ToolList};
pub use codex::{CodexLogin, CodexRun};
pub use copilot::{CopilotTokenError, validate_copilot_token};
pub use cost::{codex_cost, format_cost, merge_fraction};
pub use device_login::{
    DeviceLogin, LoginProcess, LoginSpawner, LoginSpec, LoginView, PodmanLogin, ReadBackError,
};
pub use forges::Forges;
pub use github::{
    BranchInfo, GhRepos, GitHubRepos, Permission, Protection, RepoChecker, RepoInfo, RepoReport,
    check_repo,
};
pub use mcp::{McpError, McpProjectSettings, McpServer, McpStages, McpTool, McpToolState};
pub use podman::PodmanExecutor;
pub use projects::{
    ImportError, Project, ProjectForge, Projects, StageAgents, StageEfforts, StageModels,
    import_tomls,
};
pub use proxy::{
    ProxyEvent, ProxyEventFn, RunTokens, StoreUpstreams, Upstream, Upstreams, proxy_app,
    proxy_app_with_roots,
};
pub use registry::{AgentEntry, AgentRegistry};
pub use scheduler::{
    APPROVAL, AgentRequest, ChecksRequest, Executor, FINISHED_JOB_DAYS, LogSink, McpRuntime,
    PackagingRequest, PublishRequest, Published, RENOVATE_DISCOVERY, ReadyAction, Rebase,
    RenovateWatchRequest, RerunRequest, Scheduler, SessionSink, SummaryRequest, WatchReport,
    WatchRequest, branch_name, ready_action, watch_only_event,
};
pub use secrets::{
    ClaudeToken, CodexAuthError, ForgeTokenOwner, LOCAL_BACKEND, LocalSecrets, MASTER_KEY_LEN,
    SecretError, SecretRef, SecretStore, UnavailableSecrets, master_key_from_env,
    validate_codex_auth,
};
pub use settings::Settings;
pub use store::{
    CodexPrice, Collaborator, FACTORY_REQUESTER, GapJob, JobEvent, JobFile, JobRow, LogEntry,
    Maintenance, Packaging, PackagingType, ProjectTotals, Snapshot, Store, StoreError,
};
pub use usage::PlanUsageCache;
pub use warnings::McpWarnings;
pub use web::{MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS, USER_HEADER, app, parse_admins};
pub use workers::{Slot, TaskId, WorkerSlots};
