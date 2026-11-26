//! The factory's dashboard and job store.

mod accounts;
mod check;
mod codex;
mod copilot;
mod cost;
pub mod csrf;
mod device_login;
pub mod forge_tokens;
mod forgejo;
mod forgejo_instances;
mod forges;
mod github;
mod gitlab;
mod local_server;
mod localtime;
mod mcp;
mod packages;
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

pub use accounts::{
    AccountSource, Accounts, Operation, Owner, ResolveError, Resolved, UpstreamCredential,
    UpstreamRead, blame, credential_failure, credential_reason,
};
pub use check::{CheckOutcome, HttpMcpChecker, McpChecker, TOTAL_TIMEOUT, ToolList};
pub use codex::{CodexLogin, CodexRun};
pub use copilot::{CopilotTokenError, validate_copilot_token};
pub use cost::{codex_cost, format_cost, merge_fraction};
pub use device_login::{
    DeviceLogin, LoginProcess, LoginSpawner, LoginSpec, LoginView, PodmanLogin, ReadBackError,
};
pub use forgejo_instances::{ForgejoInstance, ForgejoProbe, HttpForgejoProbe, version_warnings};
pub use forges::Forges;
pub use github::{
    BranchInfo, GhRepos, GitHubRepos, Permission, Protection, RepoChecker, RepoInfo, RepoReport,
    check_repo,
};
pub use gitlab::GlabRepos;
pub use local_server::{
    HttpLocalServerProbe, LoadError, LoadFuture, LocalServerProbe, ModelsError, ModelsFuture,
    normalize_server_url,
};
pub use mcp::{
    McpError, McpProjectSettings, McpServer, McpStages, McpTool, McpToolState, McpTransport,
};
pub use podman::PodmanExecutor;
pub use projects::{
    ImportError, Project, ProjectForge, Projects, RenovateAgent, StageAgents, StageEfforts,
    StageModels, import_tomls,
};
pub use proxy::{
    ProxyEvent, ProxyEventFn, RunTokens, StoreUpstreams, Upstream, Upstreams, proxy_app,
    proxy_app_with_roots,
};
pub use registry::{AgentEntry, AgentRegistry};
pub use scheduler::{
    APPROVAL, AgentRequest, ChecksRequest, Executor, ExistingPr, FINISHED_JOB_DAYS, LocalKeyOwner,
    LocalRequest, LogSink, McpRuntime, PackagingRequest, PublishRequest, Published, PushLog,
    RENOVATE_DISCOVERY, ReadyAction, Rebase, RenovateWatchRequest, RerunRequest, Scheduler,
    SessionSink, SummaryRequest, WatchReport, WatchRequest, branch_name, ready_action,
    watch_only_event,
};
pub use secrets::{
    ClaudeToken, CodexAuthError, ForgeTokenOwner, LOCAL_BACKEND, LocalSecrets, MASTER_KEY_LEN,
    SecretError, SecretRef, SecretStore, UnavailableSecrets, master_key_from_env,
    validate_codex_auth,
};
pub use settings::Settings;
pub use store::{
    Claim, CodexPrice, Collaborator, FACTORY_REQUESTER, GapJob, GroupsSave, JobEvent, JobFile,
    JobRow, LocalSettings, LogEntry, Maintenance, MaintenanceSave, Packaging, PackagingType,
    PrerequisiteEdit, ProjectGuard, ProjectTotals, Snapshot, Store, StoreError,
};
pub use usage::PlanUsageCache;
pub use warnings::McpWarnings;
pub use web::{
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS, USER_HEADER, app, app_with_forgejo_probe, parse_admins,
};
pub use workers::{Slot, TaskId, WorkerSlots};
