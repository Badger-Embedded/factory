use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use factory_core::{
    Agent, AgentResult, Ask, Cli, Command, DEFAULT_AGENT, DELETED_PREREQUISITE, Event, FixLink,
    FixOutcome, FixState, Gap, Gates, Job, PrerequisiteState, RebaseState, Rejected, Session,
    Stage, Turn, Wait,
};
use factory_runner::{ForgeAccount, ForgeKind, ForgejoServer, LogLine};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::forgejo_instances::ForgejoInstance;
use crate::mcp::{
    self, McpError, McpProjectSettings, McpServer, McpStages, McpTool, McpToolState, McpTransport,
};
use crate::projects::{
    Project, ProjectForge, ProjectLabel, RenovateAgent, StageAgents, StageEfforts, StageModels,
};
use crate::secrets::{ForgeTokenOwner, SecretRef};
use crate::settings::Settings;

/// The requester of a job the factory creates on its own, with no person behind it.
pub const FACTORY_REQUESTER: &str = "factory";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    name TEXT PRIMARY KEY,
    forge TEXT NOT NULL DEFAULT 'github',
    repo TEXT NOT NULL,
    base TEXT NOT NULL,
    checks TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    display_name TEXT,
    UNIQUE (forge, repo)
);
CREATE TABLE IF NOT EXISTS project_instances (
    instance INTEGER PRIMARY KEY AUTOINCREMENT,
    project TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    job TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS job_prerequisites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL,
    prerequisite INTEGER NOT NULL,
    gone TEXT CHECK (gone IN ('merged', 'unmerged')),
    title TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS job_prerequisites_live
    ON job_prerequisites (job_id, prerequisite) WHERE gone IS NULL;
CREATE TABLE IF NOT EXISTS settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    settings TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS project_models (
    project TEXT PRIMARY KEY,
    clarify TEXT,
    spec TEXT,
    plan TEXT,
    implement TEXT,
    review TEXT
);
CREATE TABLE IF NOT EXISTS project_settings (
    project TEXT PRIMARY KEY,
    auto_merge INTEGER NOT NULL DEFAULT 0,
    llm_review INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS project_gates (
    project TEXT PRIMARY KEY,
    spec INTEGER NOT NULL,
    plan INTEGER NOT NULL,
    publish INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS project_efforts (
    project TEXT PRIMARY KEY,
    clarify TEXT NOT NULL,
    spec TEXT NOT NULL,
    plan TEXT NOT NULL,
    implement TEXT NOT NULL,
    review TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS project_agents (
    project TEXT PRIMARY KEY,
    clarify TEXT NOT NULL,
    spec TEXT NOT NULL,
    plan TEXT NOT NULL,
    implement TEXT NOT NULL,
    review TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS project_renovate_agent (
    project TEXT PRIMARY KEY,
    agent TEXT NOT NULL,
    model TEXT,
    effort TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    session TEXT NOT NULL,
    uuid TEXT NOT NULL,
    agent TEXT NOT NULL DEFAULT 'claude',
    agent_session TEXT,
    cost_usd REAL,
    model TEXT,
    effort TEXT,
    local_cli TEXT,
    PRIMARY KEY (job_id, session)
);
CREATE TABLE IF NOT EXISTS logs (
    id INTEGER PRIMARY KEY,
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    kind TEXT NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS logs_by_job ON logs (job_id, id);
CREATE TABLE IF NOT EXISTS job_images (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    name TEXT NOT NULL,
    PRIMARY KEY (job_id, name)
);
CREATE TABLE IF NOT EXISTS job_files (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    name TEXT NOT NULL,
    original_name TEXT NOT NULL,
    PRIMARY KEY (job_id, name)
);
CREATE TABLE IF NOT EXISTS job_costs (
    job_id INTEGER PRIMARY KEY REFERENCES jobs (id),
    usd REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS project_totals (
    project TEXT PRIMARY KEY,
    usd REAL NOT NULL DEFAULT 0,
    jobs_created INTEGER NOT NULL DEFAULT 0,
    jobs_merged INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS job_events (
    id INTEGER PRIMARY KEY,
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    event TEXT NOT NULL,
    stage TEXT NOT NULL,
    pr INTEGER,
    turn TEXT NOT NULL,
    detail TEXT,
    usd REAL,
    estimated INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS codex_prices (
    model TEXT PRIMARY KEY,
    input_usd_per_mtok REAL NOT NULL,
    cached_input_usd_per_mtok REAL NOT NULL,
    output_usd_per_mtok REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS job_events_by_job ON job_events (job_id, id);
CREATE TABLE IF NOT EXISTS renovate_heads (
    job_id INTEGER NOT NULL,
    head TEXT NOT NULL,
    seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (job_id, head)
);
CREATE TABLE IF NOT EXISTS pr_watermarks (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    pr INTEGER NOT NULL,
    seen_until TEXT NOT NULL,
    PRIMARY KEY (job_id, pr)
);
CREATE TABLE IF NOT EXISTS secrets (
    project TEXT NOT NULL,
    server TEXT NOT NULL,
    header TEXT NOT NULL,
    backend TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (project, server, header)
);
CREATE TABLE IF NOT EXISTS mcp_servers (
    name TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    headers TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'remote',
    command TEXT NOT NULL DEFAULT '',
    args TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE IF NOT EXISTS project_mcp (
    project TEXT NOT NULL,
    server TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    clarify INTEGER NOT NULL DEFAULT 0,
    spec INTEGER NOT NULL DEFAULT 0,
    plan INTEGER NOT NULL DEFAULT 0,
    implement INTEGER NOT NULL DEFAULT 0,
    review INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (project, server)
);
CREATE TABLE IF NOT EXISTS project_mcp_tools (
    project TEXT NOT NULL,
    server TEXT NOT NULL,
    tool TEXT NOT NULL,
    description TEXT,
    offered INTEGER NOT NULL DEFAULT 1,
    disabled INTEGER NOT NULL DEFAULT 0,
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (project, server, tool)
);
CREATE TABLE IF NOT EXISTS project_mcp_tool_refreshes (
    project TEXT NOT NULL,
    server TEXT NOT NULL,
    latest INTEGER NOT NULL,
    PRIMARY KEY (project, server)
);
CREATE TABLE IF NOT EXISTS project_mcp_args (
    project TEXT NOT NULL,
    server TEXT NOT NULL,
    args TEXT NOT NULL,
    PRIMARY KEY (project, server)
);
CREATE TABLE IF NOT EXISTS project_packages (
    project TEXT PRIMARY KEY,
    packages TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS cost_closed_jobs (
    job_id INTEGER PRIMARY KEY REFERENCES jobs (id)
);
CREATE TABLE IF NOT EXISTS project_collaborators (
    project TEXT NOT NULL,
    login TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    use_my_account INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (project, login)
);
CREATE TABLE IF NOT EXISTS project_maintenance (
    project TEXT PRIMARY KEY,
    renovate INTEGER NOT NULL DEFAULT 0,
    packaging INTEGER NOT NULL DEFAULT 0,
    packaging_type TEXT,
    manifest_files TEXT NOT NULL DEFAULT '[]',
    build_config_files TEXT NOT NULL DEFAULT '[]',
    upstream_repo TEXT,
    renovate_author TEXT,
    upstream_forge TEXT
);
CREATE TABLE IF NOT EXISTS renovate_prs (
    project TEXT NOT NULL,
    number INTEGER NOT NULL,
    job_id INTEGER NOT NULL,
    PRIMARY KEY (project, number)
);
CREATE TABLE IF NOT EXISTS rebase_claims (
    job_id INTEGER NOT NULL,
    fix_job INTEGER NOT NULL,
    PRIMARY KEY (job_id, fix_job)
);
CREATE TABLE IF NOT EXISTS dismissed_gaps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL,
    project TEXT NOT NULL,
    source_job INTEGER NOT NULL,
    source_pr INTEGER NOT NULL,
    request TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS user_settings (
    login TEXT PRIMARY KEY,
    notifications INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS project_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    login TEXT NOT NULL,
    name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS project_group_members (
    login TEXT NOT NULL,
    project TEXT NOT NULL,
    group_id INTEGER NOT NULL,
    PRIMARY KEY (login, project)
);
CREATE TABLE IF NOT EXISTS user_forge_accounts (
    login TEXT NOT NULL,
    forge TEXT NOT NULL,
    account_login TEXT NOT NULL,
    account_id INTEGER NOT NULL,
    PRIMARY KEY (login, forge)
);
CREATE TABLE IF NOT EXISTS project_forge_accounts (
    project TEXT NOT NULL,
    kind TEXT NOT NULL,
    forge TEXT NOT NULL,
    account_login TEXT NOT NULL,
    account_id INTEGER NOT NULL,
    PRIMARY KEY (project, kind)
);
CREATE TABLE IF NOT EXISTS forgejo_instances (
    name TEXT PRIMARY KEY,
    base_url TEXT NOT NULL UNIQUE,
    version TEXT NOT NULL,
    added_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    registration INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS forgejo_registrations (
    id INTEGER PRIMARY KEY AUTOINCREMENT
);
CREATE TABLE IF NOT EXISTS gh_token_copies (
    project TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS local_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    url TEXT,
    cli TEXT NOT NULL DEFAULT 'codex',
    load_timeout_minutes INTEGER NOT NULL DEFAULT 10,
    models_fetched_at INTEGER,
    models_url TEXT,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS local_models (
    id TEXT PRIMARY KEY,
    position INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS store_boots (
    id INTEGER PRIMARY KEY AUTOINCREMENT
);
";

/// The longest model load wait the Local settings accept, in minutes.
pub const MAX_LOCAL_LOAD_MINUTES: u32 = 60;

/// One of a login's project groups, which sort that login's project picker
/// into sections. `projects` holds identifiers, and a project is in at most
/// one of the login's groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGroup {
    pub id: i64,
    pub name: String,
    pub projects: Vec<String>,
}

/// The Local agent's factory-wide settings. `url` is `None` until an admin
/// stores a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSettings {
    pub url: Option<String>,
    pub cli: Cli,
    pub load_timeout_minutes: u32,
    /// When the stored model list was fetched, in Unix seconds.
    pub models_fetched_at: Option<i64>,
    /// The server URL the stored model list came from, `None` when unknown.
    pub models_url: Option<String>,
}

impl Default for LocalSettings {
    fn default() -> Self {
        LocalSettings {
            url: None,
            cli: Cli::Codex,
            load_timeout_minutes: 10,
            models_fetched_at: None,
            models_url: None,
        }
    }
}

/// A file attached to a request. `name` is the generated name on disk, such
/// as `file-1.txt`; `original_name` is the browser's file name, shown on the
/// job page, used for the download name and listed in the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFile {
    pub name: String,
    pub original_name: String,
}

/// What kind of package a packaging project maintains.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PackagingType {
    #[default]
    Snap,
    Deb,
    Other,
}

impl PackagingType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Snap => "snap",
            Self::Deb => "deb",
            Self::Other => "other",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "snap" => Some(Self::Snap),
            "deb" => Some(Self::Deb),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// The packaging fields of a project that is a packaging project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packaging {
    pub kind: PackagingType,
    /// Repository paths of the packaging manifest.
    pub manifest_files: Vec<String>,
    /// Paths in the upstream repository whose changes the analysis reads.
    pub build_config_files: Vec<String>,
    /// The upstream repository, in the form `upstream_forge` names.
    pub upstream_repo: String,
    /// The forge the upstream lives on. A project that never chose one reads
    /// as its own forge.
    pub upstream_forge: ForgeKind,
}

/// A project's maintenance settings. Both switches are off by default.
/// `packaging` is `Some` exactly when "Packaging project" is on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Maintenance {
    pub renovate: bool,
    /// The login Renovate opens its pull requests as, for a forge with no
    /// fixed Renovate account. `None` on GitHub.
    pub renovate_author: Option<String>,
    pub packaging: Option<Packaging>,
}

impl Maintenance {
    /// Whether these settings may be saved.
    ///
    /// # Errors
    ///
    /// Returns the message to show when packaging is on without Renovate,
    /// without a manifest file, or with an upstream repository that is not
    /// `owner/name`.
    pub fn check(&self) -> Result<(), String> {
        let Some(packaging) = &self.packaging else {
            return Ok(());
        };
        if !self.renovate {
            return Err("A packaging project needs Renovate pull requests on: the packaging analysis runs after a merged Renovate pull request.".to_owned());
        }
        if packaging.manifest_files.is_empty() {
            return Err("Give at least one manifest file.".to_owned());
        }
        if !packaging
            .upstream_forge
            .valid_repo(&packaging.upstream_repo)
        {
            return Err(match packaging.upstream_forge {
                ForgeKind::GitHub | ForgeKind::Forgejo(_) => {
                    "The upstream repository must look like owner/name."
                }
                ForgeKind::Salsa => {
                    "The upstream repository must be a Salsa project path such as group/name."
                }
            }
            .to_owned());
        }
        Ok(())
    }
}

/// The project instance and login a token save started for. `instance` comes
/// from [`Store::project_with_instance`]. A project removed and onboarded again
/// under the same name has a new one.
pub struct ProjectGuard<'a> {
    pub project: &'a str,
    pub login: &'a str,
    pub instance: i64,
}

/// One collaborator of a project: a login that may see and act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collaborator {
    /// The collaborator's login, the value `user()` returns.
    pub login: String,
    /// The login that added this collaborator, or `migration` for a row the
    /// startup migration created.
    pub added_by: String,
    /// UTC, as SQLite's `CURRENT_TIMESTAMP` writes it.
    pub added_at: String,
    /// Whether this collaborator's request jobs run as their own forge
    /// account instead of the project's.
    pub use_my_account: bool,
}

/// What a session last ran with: its agent and that agent's model and effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSettings {
    pub agent: Agent,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// One line of a job's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// UTC, as SQLite's `CURRENT_TIMESTAMP` writes it.
    pub at: String,
    /// `text`, `tool`, `other` for agent output, or `factory`.
    pub kind: String,
    pub text: String,
}

/// Whether the factory may write a Renovate rebase request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The caller may write the request.
    Granted,
    /// An earlier call claimed it and stored no result.
    Taken,
    /// The job no longer owes this request.
    Stale,
}

/// One entry of a job's history: something that happened to the job, and
/// the stage and turn it left the job in.
#[derive(Debug, Clone, PartialEq)]
pub struct JobEvent {
    /// UTC, as SQLite's `CURRENT_TIMESTAMP` writes it.
    pub at: String,
    /// [`Event::name`], `created` when the job was stored, or `cost` when an
    /// agent run reported its cost.
    pub event: String,
    /// [`Stage::name`].
    pub stage: String,
    /// [`Stage::pr`].
    pub pr: Option<usize>,
    /// [`factory_core::Turn::name`].
    pub turn: String,
    /// The reason of a failed agent or reviewer run, or of a CI rerun.
    pub detail: Option<String>,
    /// The cost of an agent run, on `cost` entries.
    pub usd: Option<f64>,
    /// Whether this `cost` entry is an estimate the factory computed, such as a
    /// Codex run's cost from the price table, rather than a figure the agent
    /// reported. Always false on other entries.
    pub estimated: bool,
}

/// What a project has used since the totals were introduced. Deleting a job
/// does not lower them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProjectTotals {
    /// API cost of all agent runs, in USD.
    pub usd: f64,
    pub jobs_created: i64,
    /// Jobs that reached `Merged`. A job with several pull requests counts once.
    pub jobs_merged: i64,
}

/// Selects the `job_costs` rows to show as a cost: a positive total, or a zero
/// total of a job that ran a worker on Local. Another zero row reads as no cost.
const SHOWN_COST: &str = "(c.usd > 0 OR EXISTS (
    SELECT 1 FROM sessions s WHERE s.job_id = c.job_id AND s.local_cli IS NOT NULL))";

/// One model's Codex prices, in USD per million tokens. The factory estimates a
/// Codex run's cost from the row for the model it ran, since Codex reports token
/// counts and no dollar figure. An operator keeps the table on the Codex
/// settings page, so a price change needs no code change or deploy.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexPrice {
    /// The model id the run passes, such as `gpt-6.1-sol`.
    pub model: String,
    /// USD per million uncached input tokens.
    pub input_usd_per_mtok: f64,
    /// USD per million cached input tokens.
    pub cached_input_usd_per_mtok: f64,
    /// USD per million output tokens.
    pub output_usd_per_mtok: f64,
}

/// The jobs, kept in SQLite. Each job is stored as JSON in one row.
pub struct Store {
    conn: Mutex<Connection>,
    /// How many times the database has been opened, from `store_boots`.
    epoch: u64,
    reads: AtomicU64,
}

/// The jobs as one read saw them, each with the id of its latest
/// `job_events` row.
pub struct Snapshot {
    /// Orders snapshots: the number of times the database has been opened,
    /// then a count that rises with each read. A later read has a greater
    /// pair, also after a restart, and no clock is involved.
    pub order: (u64, u64),
    pub jobs: Vec<(JobRow, i64)>,
    /// What each waiting job waits for, read in the same transaction as the
    /// jobs.
    pub waits: HashMap<i64, Wait>,
}

/// A gap job as the packaging analysis sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapJob {
    pub id: i64,
    pub source_job: i64,
    pub source_pr: u64,
    pub request: String,
    pub dismissed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub id: i64,
    pub project: String,
    pub requested_by: String,
    /// UTC, as SQLite's `CURRENT_TIMESTAMP` writes it.
    pub created_at: String,
    pub job: Job,
}

#[derive(Debug)]
pub enum StoreError {
    NotFound(i64),
    Rejected(Rejected),
    /// A project with this name is already onboarded.
    DuplicateName(String),
    /// A project with this forge and repo is already onboarded.
    DuplicateRepo(ForgeKind, String),
    /// The named project is on a forge the factory does not support.
    UnknownForge(String),
    /// An effort level outside the allowed set was submitted. It is not stored.
    InvalidEffort(String),
    /// An agent id that no available agent carries was submitted for a stage.
    /// It is not stored.
    InvalidAgent(String),
    /// A stage was submitted on the Local agent with no model. The value is the
    /// stage's name. It is not stored.
    MissingLocalModel(&'static str),
    /// The Renovate agent setting was refused. The message says why. It is not
    /// stored.
    InvalidRenovateAgent(String),
    /// A request tried to remove the sole remaining collaborator of a project.
    /// The project would then be visible to nobody, so removal is refused.
    LastCollaborator,
    /// A request named a prerequisite job that does not exist, belongs to
    /// another project, or cannot be waited for. The message is the same for
    /// each, so it leaks nothing about other projects.
    InvalidPrerequisite(i64),
    /// A project's upstream forge names no fixed kind and no registered
    /// instance. Nothing reads such an upstream, as it would be another server.
    UnknownUpstreamForge(String),
    /// A Forgejo instance was refused. The message says why.
    Forgejo(String),
    /// A write named a Forgejo instance that was deleted, or registered again
    /// at another address, after the write's checks began.
    InstanceChanged(String),
    /// The Local agent's settings were refused. The message says why.
    Local(String),
    Db(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(id) => write!(f, "no job {id}"),
            StoreError::Rejected(rejected) => rejected.fmt(f),
            StoreError::DuplicateName(name) => write!(f, "a project named {name:?} already exists"),
            StoreError::DuplicateRepo(forge, repo) => {
                write!(
                    f,
                    "the {} repository {repo:?} is already onboarded",
                    forge.display_name()
                )
            }
            StoreError::UnknownForge(project) => {
                write!(
                    f,
                    "project {project:?} is on a forge the factory does not support"
                )
            }
            StoreError::UnknownUpstreamForge(name) => write!(
                f,
                "the upstream forge {name:?} is neither GitHub, Salsa nor a registered Forgejo instance"
            ),
            StoreError::InvalidEffort(effort) => {
                write!(f, "{effort:?} is not an allowed effort level")
            }
            StoreError::InvalidAgent(agent) => {
                write!(f, "{agent:?} is not an available agent")
            }
            StoreError::MissingLocalModel(stage) => write!(
                f,
                "The {stage} stage runs on Local, which needs a model. \
                 The local server picks the model from its id."
            ),
            StoreError::InvalidRenovateAgent(reason) => {
                write!(f, "The Renovate agent setting is invalid: {reason}")
            }
            StoreError::LastCollaborator => write!(
                f,
                "A project needs at least one collaborator. Remove the project instead."
            ),
            StoreError::InvalidPrerequisite(id) => {
                write!(f, "job #{id} is not a job this request can wait for")
            }
            StoreError::Forgejo(message) | StoreError::Local(message) => f.write_str(message),
            StoreError::InstanceChanged(name) => write!(
                f,
                "the Forgejo instance {name:?} was deleted or registered again while the write was checked"
            ),
            StoreError::Db(err) => write!(f, "database: {err}"),
            StoreError::Json(err) => write!(f, "stored job does not parse: {err}"),
            StoreError::Io(err) => write!(f, "file system: {err}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        StoreError::Db(err)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        StoreError::Json(err)
    }
}

/// An `mcp_servers` row's transport columns as stored.
struct StoredTransport {
    kind: String,
    url: String,
    headers: String,
    command: String,
    args: String,
}

impl StoredTransport {
    /// Validates `transport` and renders it as the columns a row stores. A
    /// stdio row keeps an empty URL and header list.
    fn validated(transport: &McpTransport) -> Result<Self, McpError> {
        Ok(match transport {
            McpTransport::Remote { url, headers } => {
                mcp::validate_url(url)?;
                let headers = mcp::normalize_headers(headers)?;
                StoredTransport {
                    kind: "remote".to_owned(),
                    url: url.clone(),
                    headers: serde_json::to_string(&headers)?,
                    command: String::new(),
                    args: "[]".to_owned(),
                }
            }
            McpTransport::Stdio { command, args } => {
                let (command, args) = mcp::validate_stdio(command, args)?;
                StoredTransport {
                    kind: "stdio".to_owned(),
                    url: String::new(),
                    headers: "[]".to_owned(),
                    command,
                    args: serde_json::to_string(&args)?,
                }
            }
        })
    }

    fn into_transport(self) -> Result<McpTransport, StoreError> {
        Ok(if self.kind == "stdio" {
            McpTransport::Stdio {
                command: self.command,
                args: serde_json::from_str(&self.args)?,
            }
        } else {
            McpTransport::Remote {
                url: self.url,
                headers: serde_json::from_str(&self.headers)?,
            }
        })
    }
}

/// Reads `kind, url, headers, command, args` from `row`, starting at `from`.
fn mcp_row(row: &rusqlite::Row<'_>, from: usize) -> rusqlite::Result<StoredTransport> {
    Ok(StoredTransport {
        kind: row.get(from)?,
        url: row.get(from + 1)?,
        headers: row.get(from + 2)?,
        command: row.get(from + 3)?,
        args: row.get(from + 4)?,
    })
}

impl Store {
    /// Opens or creates the database at `path`.
    ///
    /// # Errors
    ///
    /// Fails when SQLite cannot open the file or create the schema.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// # Errors
    ///
    /// Fails when SQLite cannot create the schema.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    /// A stdio kind joined the registry after it shipped. Old rows are remote.
    fn migrate_mcp_kind(conn: &Connection) -> Result<(), StoreError> {
        for (column, definition) in [
            ("kind", "kind TEXT NOT NULL DEFAULT 'remote'"),
            ("command", "command TEXT NOT NULL DEFAULT ''"),
            ("args", "args TEXT NOT NULL DEFAULT '[]'"),
        ] {
            let present: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('mcp_servers') WHERE name = ?1",
                [column],
                |row| row.get(0),
            )?;
            if present == 0 {
                conn.execute(
                    &format!("ALTER TABLE mcp_servers ADD COLUMN {definition}"),
                    [],
                )?;
            }
        }
        Ok(())
    }

    fn migrate_display_name(conn: &Connection) -> Result<(), StoreError> {
        let has_display_name: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('projects') WHERE name = 'display_name'",
            [],
            |row| row.get(0),
        )?;
        if has_display_name == 0 {
            conn.execute("ALTER TABLE projects ADD COLUMN display_name TEXT", [])?;
        }
        Ok(())
    }

    /// `models_url` joined `local_settings` after it shipped. A list stored
    /// before then has no known source, so the column stays empty until the
    /// next successful fetch.
    fn migrate_local_models_url(conn: &Connection) -> Result<(), StoreError> {
        let has_column: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('local_settings') WHERE name = 'models_url'",
            [],
            |row| row.get(0),
        )?;
        if has_column == 0 {
            conn.execute("ALTER TABLE local_settings ADD COLUMN models_url TEXT", [])?;
        }
        Ok(())
    }

    /// `registration` joined `forgejo_instances` after it shipped. Every
    /// instance without one gets a fresh number.
    fn migrate_forgejo_registration(conn: &Connection) -> Result<(), StoreError> {
        let has_column: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('forgejo_instances') WHERE name = 'registration'",
            [],
            |row| row.get(0),
        )?;
        if has_column == 0 {
            conn.execute(
                "ALTER TABLE forgejo_instances ADD COLUMN registration INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        let unnumbered: Vec<String> = {
            let mut stmt =
                conn.prepare("SELECT name FROM forgejo_instances WHERE registration = 0")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        for name in unnumbered {
            conn.execute("INSERT INTO forgejo_registrations DEFAULT VALUES", [])?;
            conn.execute(
                "UPDATE forgejo_instances SET registration = last_insert_rowid() WHERE name = ?1",
                [name],
            )?;
        }
        Ok(())
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA)?;
        Self::migrate_forgejo_registration(&conn)?;
        Self::migrate_local_models_url(&conn)?;
        Self::migrate_project_forge(&conn)?;
        Self::migrate_renovate_author(&conn)?;
        Self::migrate_upstream_forge(&conn)?;
        Self::migrate_display_name(&conn)?;
        // `review` joined the model stages after the table shipped, so add it to
        // a `project_models` table that predates it. A fresh database already
        // has the column from SCHEMA, so this skips it.
        let has_review: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('project_models') WHERE name = 'review'",
            [],
            |row| row.get(0),
        )?;
        if has_review == 0 {
            conn.execute("ALTER TABLE project_models ADD COLUMN review TEXT", [])?;
        }
        Self::migrate_mcp_kind(&conn)?;
        let has_use_my_account: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('project_collaborators') WHERE name = 'use_my_account'",
            [],
            |row| row.get(0),
        )?;
        if has_use_my_account == 0 {
            conn.execute(
                "ALTER TABLE project_collaborators ADD COLUMN use_my_account INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        // `llm_review` joined `project_settings` after auto-merge shipped, so
        // add it to a table that predates it. A fresh database already has the
        // column from SCHEMA. The default is off, so existing projects stay off.
        let has_llm_review: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('project_settings') WHERE name = 'llm_review'",
            [],
            |row| row.get(0),
        )?;
        if has_llm_review == 0 {
            conn.execute(
                "ALTER TABLE project_settings ADD COLUMN llm_review INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        // `agent` joined `sessions` when agents became pluggable, so add it to a
        // table that predates it. A fresh database already has the column from
        // SCHEMA. The default is `claude`, so a session started before the column
        // existed resumes on Claude, the one agent that ran then.
        let has_agent: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'agent'",
            [],
            |row| row.get(0),
        )?;
        if has_agent == 0 {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude'",
                [],
            )?;
        }
        // `agent_session` holds the id the agent reports for its own session,
        // such as Codex's thread id, added to a table that predates it. It stays
        // null for a session Claude runs, which resumes by the factory's UUID, so
        // an old row reads null and keeps resuming on the UUID in `uuid`.
        let has_agent_session: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'agent_session'",
            [],
            |row| row.get(0),
        )?;
        if has_agent_session == 0 {
            conn.execute("ALTER TABLE sessions ADD COLUMN agent_session TEXT", [])?;
        }
        // `cost_usd` holds the session total the agent last reported, added to
        // a table that predates it. A row from before reads null, so its next
        // run counts the whole total once.
        let has_session_cost: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'cost_usd'",
            [],
            |row| row.get(0),
        )?;
        if has_session_cost == 0 {
            conn.execute("ALTER TABLE sessions ADD COLUMN cost_usd REAL", [])?;
        }
        // `local_cli` records the CLI a Local session started on, so a run that
        // resumes it keeps that CLI after the setting changes. It stays NULL for
        // every session that is not Local.
        for column in ["model", "effort", "local_cli"] {
            let has: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = ?1",
                [column],
                |row| row.get(0),
            )?;
            if has == 0 {
                conn.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {column} TEXT"),
                    [],
                )?;
            }
        }
        // `estimated` marks a cost row the factory estimated rather than read
        // from the agent, such as a Codex run's cost from the price table, so the
        // job page can flag a total that includes an estimate. It joined
        // `job_events` after the table shipped, so add it to one that predates it.
        // A fresh database already has the column from SCHEMA. The default is 0,
        // so every earlier row, all of them reported costs, reads as not
        // estimated.
        let has_estimated: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('job_events') WHERE name = 'estimated'",
            [],
            |row| row.get(0),
        )?;
        if has_estimated == 0 {
            conn.execute(
                "ALTER TABLE job_events ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        conn.execute("INSERT INTO store_boots DEFAULT VALUES", [])?;
        let epoch = u64::try_from(conn.last_insert_rowid()).unwrap_or(0);
        Ok(Store {
            conn: Mutex::new(conn),
            epoch,
            reads: AtomicU64::new(0),
        })
    }

    /// A NULL `upstream_forge` reads as the project's own forge, so no row
    /// needs a value.
    fn migrate_upstream_forge(conn: &Connection) -> Result<(), StoreError> {
        let has: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('project_maintenance') WHERE name = 'upstream_forge'",
            [],
            |row| row.get(0),
        )?;
        if has == 0 {
            conn.execute(
                "ALTER TABLE project_maintenance ADD COLUMN upstream_forge TEXT",
                [],
            )?;
        }
        Ok(())
    }

    fn migrate_renovate_author(conn: &Connection) -> Result<(), StoreError> {
        let has: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('project_maintenance') WHERE name = 'renovate_author'",
            [],
            |row| row.get(0),
        )?;
        if has == 0 {
            conn.execute(
                "ALTER TABLE project_maintenance ADD COLUMN renovate_author TEXT",
                [],
            )?;
        }
        Ok(())
    }

    /// `forge` joined `projects` after the table shipped with `repo` unique on
    /// its own. SQLite cannot change a constraint in place, so a table without
    /// the column is rebuilt with every row on `github`. A fresh database
    /// already has the column from SCHEMA, so this skips it.
    fn migrate_project_forge(conn: &Connection) -> Result<(), StoreError> {
        let has_forge: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('projects') WHERE name = 'forge'",
            [],
            |row| row.get(0),
        )?;
        if has_forge > 0 {
            return Ok(());
        }
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE projects_new (
                 name TEXT PRIMARY KEY,
                 forge TEXT NOT NULL DEFAULT 'github',
                 repo TEXT NOT NULL,
                 base TEXT NOT NULL,
                 checks TEXT,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                 UNIQUE (forge, repo)
             );
             INSERT INTO projects_new (name, forge, repo, base, checks, created_at)
                 SELECT name, 'github', repo, base, checks, created_at FROM projects;
             DROP TABLE projects;
             ALTER TABLE projects_new RENAME TO projects;
             COMMIT;",
        )?;
        Ok(())
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every onboarded project with its name, in name order. The models and
    /// the auto-merge and LLM-review flags live in `project_models` and
    /// `project_settings`, so each project carries those at their defaults.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn list_projects(&self) -> Result<Vec<(String, Project)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, forge, repo, base, checks,
                    (SELECT base_url FROM forgejo_instances WHERE name = projects.forge),
                    (SELECT registration FROM forgejo_instances WHERE name = projects.forge)
             FROM projects ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, project_row(row)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The project named `name`, or `None` when none is onboarded under it.
    /// As [`Store::list_projects`], its models and flags read at their
    /// defaults.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project(&self, name: &str) -> Result<Option<Project>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT forge, repo, base, checks,
                        (SELECT base_url FROM forgejo_instances WHERE name = projects.forge),
                        (SELECT registration FROM forgejo_instances WHERE name = projects.forge)
                 FROM projects WHERE name = ?1",
                [name],
                project_row_without_name,
            )
            .optional()?)
    }

    /// Onboards a project with its `forge`, `repo`, `base` and optional `checks`. Its
    /// approval gates start as a copy of the default gates in the settings, so
    /// a later change to the defaults leaves the project alone. Its models and
    /// flags start at their defaults; the settings page saves them into
    /// `project_models` and `project_settings`.
    ///
    /// `by` is the onboarding login, added as the project's first collaborator
    /// in the same transaction. It is `None` only for the toml import, whose
    /// projects the collaborator migration seeds from their jobs instead.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::DuplicateName`] when `name` is taken, with
    /// [`StoreError::DuplicateRepo`] when `repo` is already onboarded on `forge`, and on
    /// a database error. Nothing is written when it fails.
    pub fn create_project(
        &self,
        name: &str,
        forge: &ForgeKind,
        repo: &str,
        base: &str,
        checks: Option<&str>,
        by: Option<&str>,
    ) -> Result<(), StoreError> {
        self.insert_project(name, forge, repo, base, checks, by, false, false)
    }

    /// As [`Store::create_project`], with `use_my_account` as the first
    /// collaborator's "Use my account" switch. Onboarding turns it on, because
    /// its checks just showed that login's account can push. It has no effect
    /// when `by` is `None`.
    ///
    /// # Errors
    ///
    /// As [`Store::create_project`], and with [`StoreError::InstanceChanged`]
    /// when `forge` is a Forgejo instance that was deleted or registered again.
    #[allow(clippy::too_many_arguments)]
    pub fn create_project_as(
        &self,
        name: &str,
        forge: &ForgeKind,
        repo: &str,
        base: &str,
        checks: Option<&str>,
        by: Option<&str>,
        use_my_account: bool,
    ) -> Result<(), StoreError> {
        self.insert_project(name, forge, repo, base, checks, by, use_my_account, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_project(
        &self,
        name: &str,
        forge: &ForgeKind,
        repo: &str,
        base: &str,
        checks: Option<&str>,
        by: Option<&str>,
        use_my_account: bool,
        require_instance: bool,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if require_instance {
            require_registered(&tx, forge)?;
        }
        let name_taken = tx
            .query_row(
                "SELECT 1 FROM projects WHERE name = ?1 OR display_name = ?1",
                [name],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if name_taken {
            return Err(StoreError::DuplicateName(name.to_owned()));
        }
        let repo_taken = tx
            .query_row(
                "SELECT 1 FROM projects WHERE forge = ?1 AND repo = ?2",
                [forge.as_str(), repo],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if repo_taken {
            return Err(StoreError::DuplicateRepo(forge.clone(), repo.to_owned()));
        }
        tx.execute(
            "INSERT INTO projects (name, forge, repo, base, checks) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, forge.as_str(), repo, base, checks],
        )?;
        let gates = Self::settings_in(&tx)?.gates;
        Self::insert_project_gates(&tx, name, gates)?;
        if let Some(by) = by {
            tx.execute(
                "INSERT INTO project_collaborators (project, login, added_by, use_my_account)
                 VALUES (?1, ?2, ?2, ?3)",
                params![name, by, use_my_account],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO gh_token_copies (project) VALUES (?1)",
                [name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether the one-time copy of `GH_TOKEN` is done for `project`, or does
    /// not apply to it.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn gh_token_copy_done(&self, project: &str) -> Result<bool, StoreError> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM gh_token_copies WHERE project = ?1)",
            [project],
            |row| row.get(0),
        )?)
    }

    /// Records that `project` needs no more copies of `GH_TOKEN`.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn record_gh_token_copy(&self, project: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO gh_token_copies (project) VALUES (?1)",
            [project],
        )?;
        Ok(())
    }

    /// # Errors
    ///
    /// Fails on a database error.
    pub fn create(
        &self,
        project: &str,
        requested_by: &str,
        request: &str,
    ) -> Result<i64, StoreError> {
        self.create_after(project, requested_by, request, &[])
    }

    /// Creates a job that starts only after every job in `after` has merged.
    /// The job and its `job_prerequisites` rows go in one transaction. A
    /// repeated id counts once.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::InvalidPrerequisite`] when an id names no job
    /// of `project`, or a Renovate job or gap proposal, and then creates
    /// nothing. The state of a valid prerequisite does not matter. Fails on a
    /// database error.
    pub fn create_after(
        &self,
        project: &str,
        requested_by: &str,
        request: &str,
        after: &[i64],
    ) -> Result<i64, StoreError> {
        let gates = self.project_gates(project)?;
        let new = Job::new(request, gates);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut ids = after.to_vec();
        ids.sort_unstable();
        ids.dedup();
        for &prerequisite in &ids {
            match Self::load(&tx, prerequisite) {
                Ok(row) if row.project == project && row.job.accepts_as_prerequisite() => {}
                Ok(_) | Err(StoreError::NotFound(_)) => {
                    return Err(StoreError::InvalidPrerequisite(prerequisite));
                }
                Err(err) => return Err(err),
            }
        }
        let id = Self::insert_job(&tx, project, requested_by, &new)?;
        for prerequisite in ids {
            tx.execute(
                "INSERT INTO job_prerequisites (job_id, prerequisite) VALUES (?1, ?2)",
                params![id, prerequisite],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Creates the job that triages Renovate pull request `number` of
    /// `project`. The requester is `factory`. The `renovate_prs` row and the
    /// job go in one transaction, so a pull request never gets a second job,
    /// across restarts too. Returns `None` when the pull request already has
    /// one, even when that job is finished.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn create_renovate_job(
        &self,
        project: &str,
        number: u64,
        title: &str,
        url: &str,
    ) -> Result<Option<i64>, StoreError> {
        let new = Job::renovate(number, title, url);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let number = i64::try_from(number).unwrap_or(i64::MAX);
        let known = tx
            .query_row(
                "SELECT 1 FROM renovate_prs WHERE project = ?1 AND number = ?2",
                params![project, number],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if known {
            return Ok(None);
        }
        let id = Self::insert_job(&tx, project, FACTORY_REQUESTER, &new)?;
        tx.execute(
            "INSERT INTO renovate_prs (project, number, job_id) VALUES (?1, ?2, ?3)",
            params![project, number, id],
        )?;
        tx.commit()?;
        Ok(Some(id))
    }

    /// Creates a gap job in `project`, found by job `source_job` after
    /// Renovate pull request `source_pr` merged. The requester is `factory`,
    /// and the job waits for approval with the project's current gates.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn create_gap_job(
        &self,
        project: &str,
        source_job: i64,
        source_pr: u64,
        text: &str,
    ) -> Result<i64, StoreError> {
        let gates = self.project_gates(project)?;
        let new = Job::proposal(text, gates, source_job, source_pr);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let id = Self::insert_job(&tx, project, FACTORY_REQUESTER, &new)?;
        tx.commit()?;
        Ok(id)
    }

    /// Creates a job for each of `gaps`, with `range` as the upstream versions
    /// in its text, and records their ids on the Renovate job `renovate_id` in one transaction, so a retry never creates a gap
    /// twice. An empty list is valid.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Rejected`] when the Renovate job is not at the
    /// packaging step, and then nothing is saved.
    pub fn propose_gaps(
        &self,
        renovate_id: i64,
        gaps: &[Gap],
        range: Option<&(String, String)>,
    ) -> Result<JobRow, StoreError> {
        let project = self.get(renovate_id)?.project;
        let gates = self.project_gates(&project)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut row = Self::load(&tx, renovate_id)?;
        let mut check = row.job.clone();
        check
            .apply(Event::GapsProposed { jobs: Vec::new() })
            .map_err(StoreError::Rejected)?;
        let number = row.job.renovate_pr().map(|pr| pr.number).ok_or_else(|| {
            StoreError::Rejected(Rejected::Unexpected {
                stage: row.job.stage().clone(),
                event: Box::new(Event::GapsProposed { jobs: Vec::new() }),
            })
        })?;
        let mut jobs = Vec::new();
        for gap in gaps {
            let text = gap.request(number, range.map(|(a, b)| (a.as_str(), b.as_str())));
            let new = Job::proposal(&text, gates, renovate_id, number);
            jobs.push(Self::insert_job(&tx, &project, FACTORY_REQUESTER, &new)?);
        }
        Self::apply_in(&tx, &mut row, Event::GapsProposed { jobs })?;
        tx.commit()?;
        Ok(row)
    }

    /// Claims the one request to Renovate to rebase for fix job `fix_job` of
    /// Renovate job `job_id`, in the transaction that checks the job still
    /// owes that request for `head`. A claim that exists means an earlier call
    /// may have written, so a restart between the write and its result never
    /// writes twice.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn claim_rebase(&self, job_id: i64, fix_job: i64, head: &str) -> Result<Claim, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row = Self::load(&tx, job_id)?;
        // The job's own next command says the request is still due, which a
        // cancelled job's leftover fix link does not.
        let owed = matches!(
            row.job.renovate_pr().and_then(|pr| pr.fix.as_ref()),
            Some(FixLink {
                job,
                state: FixState::Merged {
                    head: owed,
                    rebase: RebaseState::Pending,
                },
                ..
            }) if *job == fix_job && owed == head
        ) && matches!(
            row.job.next_command(),
            Some(Command::RequestRebase { head: due, .. }) if due == head
        );
        if !owed {
            return Ok(Claim::Stale);
        }
        let claimed = tx.execute(
            "INSERT OR IGNORE INTO rebase_claims (job_id, fix_job) VALUES (?1, ?2)",
            params![job_id, fix_job],
        )?;
        tx.commit()?;
        Ok(if claimed == 1 {
            Claim::Granted
        } else {
            Claim::Taken
        })
    }

    /// Drops the claim of Renovate job `job_id`, for a request the forge
    /// refused before any write.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn release_rebase_claim(&self, job_id: i64) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM rebase_claims WHERE job_id = ?1", [job_id])?;
        Ok(())
    }

    /// When the factory first saw `head` on Renovate job `job_id`'s pull
    /// request, as SQLite's `CURRENT_TIMESTAMP` writes it. The head was the
    /// head by then.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn head_first_seen(&self, job_id: i64, head: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT seen_at FROM renovate_heads WHERE job_id = ?1 AND head = ?2",
                params![job_id, head],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Creates the fix job for the stopped Renovate job `renovate_id`, with
    /// `login` as its requester, and links it to the Renovate job in one
    /// transaction. A second call while the first fix job is unfinished is
    /// rejected and creates nothing.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Rejected`] when the Renovate job is not at
    /// Triage stopped or already waits for a fix job.
    pub fn start_fix(
        &self,
        renovate_id: i64,
        login: &str,
        instructions: Option<&str>,
    ) -> Result<i64, StoreError> {
        let project = self.get(renovate_id)?.project;
        let gates = self.project_gates(&project)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut row = Self::load(&tx, renovate_id)?;
        let mut check = row.job.clone();
        check
            .apply(Event::FixStarted { job: 0 })
            .map_err(StoreError::Rejected)?;
        let (Some(pr), Turn::User(Ask::Renovate { reason, .. })) =
            (row.job.renovate_pr(), row.job.turn())
        else {
            return Err(StoreError::Rejected(Rejected::Unexpected {
                stage: row.job.stage().clone(),
                event: Box::new(Event::FixStarted { job: 0 }),
            }));
        };
        let text = pr.fix_request(reason, instructions);
        let new = Job::fix(text, gates, renovate_id, pr.number);
        let id = Self::insert_job(&tx, &project, login, &new)?;
        Self::apply_in(&tx, &mut row, Event::FixStarted { job: id })?;
        tx.commit()?;
        Ok(id)
    }

    /// Approves the gap job `id` for `login`. The job moves to `requested`
    /// and `login` becomes its requester in one transaction, so the two never
    /// disagree.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Rejected`] when the job is not waiting for
    /// approval, and then nothing is saved.
    pub fn approve_proposal(&self, id: i64, login: &str) -> Result<JobRow, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut row = Self::load(&tx, id)?;
        if *row.job.stage() != Stage::Proposed {
            return Err(StoreError::Rejected(Rejected::Unexpected {
                stage: row.job.stage().clone(),
                event: Box::new(Event::Approve),
            }));
        }
        Self::apply_in(&tx, &mut row, Event::Approve)?;
        tx.execute(
            "UPDATE jobs SET requested_by = ?1 WHERE id = ?2",
            params![login, id],
        )?;
        login.clone_into(&mut row.requested_by);
        tx.commit()?;
        Ok(row)
    }

    /// The gap jobs of `project` that are waiting for approval, still
    /// running or dismissed. Live jobs come first, newest first, then the
    /// dismissals, newest first. A dismissal is kept in its own table under
    /// an identity of its own, because SQLite may reuse a deleted job's id.
    /// It outlives the job's expiry, so a dismissed gap is not proposed
    /// again. A gap job that ended another way is not listed.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored job that does not parse.
    pub fn gap_jobs(&self, project: &str) -> Result<Vec<GapJob>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, project, requested_by, created_at, job FROM jobs
             WHERE project = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([project], raw_row)?;
        let mut gaps = Vec::new();
        for row in rows {
            let row = parse_row(row?)?;
            let Some((source_job, source_pr)) = row.job.gap_source() else {
                continue;
            };
            if !row.job.is_finished() {
                gaps.push(GapJob {
                    id: row.id,
                    source_job,
                    source_pr,
                    request: row.job.request().to_owned(),
                    dismissed: false,
                });
            }
        }
        let mut stmt = conn.prepare(
            "SELECT job_id, source_job, source_pr, request FROM dismissed_gaps
             WHERE project = ?1 ORDER BY id DESC",
        )?;
        let kept = stmt.query_map([project], |r| {
            Ok(GapJob {
                id: r.get(0)?,
                source_job: r.get(1)?,
                source_pr: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                request: r.get(3)?,
                dismissed: true,
            })
        })?;
        for gap in kept {
            gaps.push(gap?);
        }
        Ok(gaps)
    }

    /// Inserts `job` with its `created` event and counts it in the project's
    /// totals.
    fn insert_job(
        tx: &Transaction<'_>,
        project: &str,
        requested_by: &str,
        job: &Job,
    ) -> Result<i64, StoreError> {
        tx.execute(
            "INSERT INTO jobs (project, requested_by, job) VALUES (?1, ?2, ?3)",
            params![project, requested_by, serde_json::to_string(job)?],
        )?;
        let id = tx.last_insert_rowid();
        Self::record(tx, id, "created", job, None, None)?;
        tx.execute(
            "INSERT INTO project_totals (project, jobs_created) VALUES (?1, 1)
             ON CONFLICT (project) DO UPDATE SET jobs_created = jobs_created + 1",
            [project],
        )?;
        Ok(id)
    }

    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`.
    pub fn get(&self, id: i64) -> Result<JobRow, StoreError> {
        Self::load(&self.conn(), id)
    }

    /// All jobs, newest first.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored job that does not parse.
    pub fn list(&self) -> Result<Vec<JobRow>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, project, requested_by, created_at, job FROM jobs ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], raw_row)?;
        rows.map(|row| parse_row(row?)).collect()
    }

    /// All jobs, newest first, each with the id of its latest `job_events`
    /// row, or 0 when it has none. Both come from one read transaction, so a
    /// job's state and its event id always match.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored job that does not parse.
    pub fn list_with_latest_event(&self) -> Result<Snapshot, StoreError> {
        self.list_with_latest_event_between(|| {})
    }

    /// [`Store::list_with_latest_event`] that runs `between` after the jobs
    /// are read and before the event ids are. Tests use it to change a job at
    /// that point, from another connection.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored job that does not parse.
    #[doc(hidden)]
    pub fn list_with_latest_event_between(
        &self,
        between: impl FnOnce(),
    ) -> Result<Snapshot, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let jobs = {
            let mut stmt = tx.prepare(
                "SELECT id, project, requested_by, created_at, job FROM jobs ORDER BY id DESC",
            )?;
            let rows = stmt.query_map([], raw_row)?;
            rows.map(|row| parse_row(row?))
                .collect::<Result<Vec<_>, _>>()?
        };
        between();
        let events: HashMap<i64, i64> = {
            let mut stmt = tx.prepare("SELECT job_id, MAX(id) FROM job_events GROUP BY job_id")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let waits = Self::waits_in(&tx, None)?;
        tx.commit()?;
        let jobs = jobs
            .into_iter()
            .map(|row| {
                let event = events.get(&row.id).copied().unwrap_or(0);
                (row, event)
            })
            .collect();
        Ok(Snapshot {
            order: (self.epoch, self.reads.fetch_add(1, Ordering::SeqCst) + 1),
            jobs,
            waits,
        })
    }

    /// The ids of jobs created more than `days` days ago.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn ids_older_than(&self, days: u32) -> Result<Vec<i64>, StoreError> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id FROM jobs WHERE created_at < datetime('now', ?1) ORDER BY id")?;
        let rows = stmt.query_map([format!("-{days} days")], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Deletes job `id` with its rows in every table and its directory
    /// `<data_dir>/jobs/<id>`. A directory that is already gone is fine.
    /// When the directory cannot be removed, the rows stay, so a later call
    /// can finish the job.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`, and on
    /// a file system or database error.
    pub fn delete_job(&self, id: i64, data_dir: &Path) -> Result<(), StoreError> {
        let row = Self::load(&self.conn(), id)?;
        match std::fs::remove_dir_all(data_dir.join("jobs").join(id.to_string())) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(StoreError::Io(err));
            }
            _ => {}
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for table in [
            "job_images",
            "job_files",
            "job_costs",
            "job_events",
            "pr_watermarks",
            "logs",
            "sessions",
            "cost_closed_jobs",
            "rebase_claims",
            "renovate_heads",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE job_id = ?1"), [id])?;
        }
        // SQLite reuses a deleted job's id, so a job that waited for this one
        // keeps its own record of how it ended instead of the id.
        tx.execute(
            "UPDATE job_prerequisites SET gone = ?2, title = ?3
             WHERE prerequisite = ?1 AND gone IS NULL",
            params![
                id,
                if *row.job.stage() == Stage::Merged {
                    "merged"
                } else {
                    "unmerged"
                },
                row.job.request().lines().next().unwrap_or_default()
            ],
        )?;
        tx.execute("DELETE FROM job_prerequisites WHERE job_id = ?1", [id])?;
        tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        Ok(tx.commit()?)
    }

    /// Deletes the `work/` checkout and the `claude/` and `codex/` session
    /// directories of merged job `id` under `<data_dir>/jobs/<id>`, to free
    /// disk, and keeps everything else: the job row, its other tables, and the
    /// `images/` and `files/` directories. Returns whether any directory existed
    /// and was removed, so the caller logs the cleanup only on the pass that
    /// freed something. A directory that is already gone counts as success and
    /// is not reported as removed.
    ///
    /// # Errors
    ///
    /// Fails on a file system error. Any directory removed before the error
    /// stays gone, so a later call finishes the cleanup.
    pub fn clean_merged_job(&self, id: i64, data_dir: &Path) -> Result<bool, StoreError> {
        let dir = data_dir.join("jobs").join(id.to_string());
        let mut removed = false;
        for sub in ["work", "claude", "codex"] {
            match std::fs::remove_dir_all(dir.join(sub)) {
                Ok(()) => removed = true,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(StoreError::Io(err)),
            }
        }
        Ok(removed)
    }

    /// Applies `event` to job `id` and saves the result in one transaction.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Rejected`] when the job rejects the event, and
    /// then nothing is saved.
    pub fn apply(&self, id: i64, event: Event) -> Result<JobRow, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut row = Self::load(&tx, id)?;
        Self::apply_in(&tx, &mut row, event)?;
        tx.commit()?;
        Ok(row)
    }

    /// Applies [`Event::Start`] to requested job `id` when every job it waits
    /// for has merged, and returns the started job. Returns `None` and changes
    /// nothing while any prerequisite is unmet. The check and the start share
    /// one transaction under the connection lock, so an edit of the job's
    /// request cannot land between them.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Rejected`] when the job rejects the event.
    pub fn start(&self, id: i64) -> Result<Option<JobRow>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut row = Self::load(&tx, id)?;
        if Self::waits_in(&tx, Some(id))?.remove(&id).is_some() {
            return Ok(None);
        }
        Self::apply_in(&tx, &mut row, Event::Start)?;
        tx.commit()?;
        Ok(Some(row))
    }

    /// What each job with unmet prerequisites waits for. A job whose
    /// prerequisites have all merged, or that has none, is absent.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored job that does not parse.
    pub fn waits(&self) -> Result<HashMap<i64, Wait>, StoreError> {
        Self::waits_in(&self.conn(), None)
    }

    /// The prerequisites of job `id` in the order they were chosen, each
    /// with its current state.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored job that does not parse.
    pub fn prerequisites(&self, id: i64) -> Result<Vec<PrerequisiteView>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT p.prerequisite, p.gone, p.title, j.job, p.id
             FROM job_prerequisites p
             LEFT JOIN jobs j ON p.gone IS NULL AND j.id = p.prerequisite
             WHERE p.job_id = ?1
             ORDER BY p.id",
        )?;
        let rows = stmt.query_map([id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut views = Vec::new();
        for row in rows {
            let (prerequisite, gone, title, job, row_id) = row?;
            let job = job
                .map(|json| serde_json::from_str::<Job>(&json))
                .transpose()?;
            let state = match (gone.as_deref(), &job) {
                (Some("merged"), _) => PrerequisiteState::Met,
                (_, Some(job)) => job.prerequisite_state(),
                (_, None) => PrerequisiteState::Blocked,
            };
            let title = match &job {
                Some(job) => job.request().lines().next().unwrap_or_default().to_owned(),
                None => title.unwrap_or_default(),
            };
            views.push(PrerequisiteView {
                row: row_id,
                id: prerequisite,
                title,
                state,
                job,
            });
        }
        Ok(views)
    }

    fn waits_in(conn: &Connection, only: Option<i64>) -> Result<HashMap<i64, Wait>, StoreError> {
        let mut stmt = conn.prepare(
            "SELECT p.job_id, p.prerequisite, p.gone, j.job
             FROM job_prerequisites p
             LEFT JOIN jobs j ON p.gone IS NULL AND j.id = p.prerequisite
             WHERE ?1 IS NULL OR p.job_id = ?1
             ORDER BY p.job_id, p.id",
        )?;
        let rows = stmt.query_map([only], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut grouped: Vec<(i64, Vec<PrerequisiteEntry>)> = Vec::new();
        for row in rows {
            let (job_id, prerequisite, gone, job) = row?;
            let entry = match (gone.as_deref(), job) {
                (Some("merged"), _) => (prerequisite, PrerequisiteState::Met, "merged".into()),
                (Some(_), _) | (None, None) => (
                    prerequisite,
                    PrerequisiteState::Blocked,
                    DELETED_PREREQUISITE.into(),
                ),
                (None, Some(json)) => {
                    let job: Job = serde_json::from_str(&json)?;
                    let label = match job.stage() {
                        Stage::Failed { .. } => "failed",
                        Stage::Cancelled => "cancelled",
                        Stage::Dismissed => "dismissed",
                        _ => "",
                    };
                    (prerequisite, job.prerequisite_state(), label.into())
                }
            };
            match grouped.last_mut() {
                Some((id, entries)) if *id == job_id => entries.push(entry),
                _ => grouped.push((job_id, vec![entry])),
            }
        }
        Ok(grouped
            .into_iter()
            .filter_map(|(id, entries)| match Wait::fold(entries) {
                Wait::Ready => None,
                wait => Some((id, wait)),
            })
            .collect())
    }

    /// Applies `event` to `row`, saves the job, adds the event to its history
    /// and counts a first arrival at `Merged` in the project totals. An
    /// `Event::RenovateObserved` that leaves the job unchanged does none of
    /// these.
    fn apply_in(tx: &Transaction<'_>, row: &mut JobRow, event: Event) -> Result<(), StoreError> {
        let was_merged = *row.job.stage() == Stage::Merged;
        let name = event.name();
        let detail = match &event {
            Event::AgentFailed { reason }
            | Event::ReviewFailed { reason }
            | Event::RebaseFailed { reason }
            | Event::Agent(AgentResult::RerunCi { reason }) => Some(reason.clone()),
            Event::ChecksError { error } => Some(error.clone()),
            Event::FixStarted { job } => Some(job.to_string()),
            Event::FixEnded { job, outcome } => Some(format!(
                "{job}: {}",
                match outcome {
                    FixOutcome::Merged { .. } | FixOutcome::MergedUnordered { .. } => "merged",
                    FixOutcome::Cancelled => "was cancelled",
                    FixOutcome::Closed => "was closed without merging",
                    FixOutcome::Dismissed => "failed and was dismissed",
                    FixOutcome::Deleted => "was deleted",
                }
            )),
            _ => None,
        };
        let observed = matches!(
            event,
            Event::RenovateObserved { .. } | Event::FixEnded { .. }
        );
        // A stored result or a new fix retires the claims, so one left behind
        // never blocks the request of a later fix that reuses the id.
        let retires_claims = matches!(
            event,
            Event::FixStarted { .. }
                | Event::RebaseRequested
                | Event::RebaseSkipped
                | Event::RebaseFailed { .. }
        );
        let before = observed.then(|| row.job.clone());
        row.job.apply(event).map_err(StoreError::Rejected)?;
        if let (true, Some(head)) = (
            observed,
            row.job.renovate_pr().and_then(|pr| pr.head.as_deref()),
        ) {
            tx.execute(
                "INSERT OR IGNORE INTO renovate_heads (job_id, head) VALUES (?1, ?2)",
                params![row.id, head],
            )?;
        }
        if retires_claims {
            tx.execute("DELETE FROM rebase_claims WHERE job_id = ?1", [row.id])?;
        }
        if before.is_some_and(|before| before == row.job) {
            return Ok(());
        }
        Self::record(tx, row.id, name, &row.job, detail.as_deref(), None)?;
        tx.execute(
            "UPDATE jobs SET job = ?1 WHERE id = ?2",
            params![serde_json::to_string(&row.job)?, row.id],
        )?;
        if let (Stage::Dismissed, Some((source_job, source_pr))) =
            (row.job.stage(), row.job.gap_source())
        {
            tx.execute(
                "INSERT INTO dismissed_gaps
                 (job_id, project, source_job, source_pr, request)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    row.id,
                    row.project,
                    source_job,
                    i64::try_from(source_pr).unwrap_or(i64::MAX),
                    row.job.request()
                ],
            )?;
        }
        if !was_merged && *row.job.stage() == Stage::Merged {
            tx.execute(
                "INSERT INTO project_totals (project, jobs_merged) VALUES (?1, 1)
                 ON CONFLICT (project) DO UPDATE SET jobs_merged = jobs_merged + 1",
                [&row.project],
            )?;
        }
        Ok(())
    }

    /// Records the file names of the images saved for job `id`. The bytes
    /// live under the data directory.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn add_images(&self, id: i64, names: &[String]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for name in names {
            tx.execute(
                "INSERT INTO job_images (job_id, name) VALUES (?1, ?2)",
                params![id, name],
            )?;
        }
        Ok(tx.commit()?)
    }

    /// Records the generated name and original name of each file saved for
    /// job `id`, in upload order. The bytes live under the data directory.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn add_files(&self, id: i64, files: &[JobFile]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for file in files {
            tx.execute(
                "INSERT INTO job_files (job_id, name, original_name) VALUES (?1, ?2, ?3)",
                params![id, file.name, file.original_name],
            )?;
        }
        Ok(tx.commit()?)
    }

    /// Replaces the request text, attachment set and prerequisite set of a
    /// still-queued job in one transaction, so the scheduler's `Event::Start` cannot run in
    /// between. The caller writes the new attachment set to the job's
    /// `staging/` directory first, numbered as `write_numbered_attachments`
    /// does, and passes the resulting `image_names` and `files`. Holding the
    /// connection lock, this checks the job is still `requested`, swaps the
    /// staged set in over the old one, replaces the `job_images` and
    /// `job_files` rows, replaces the `job_prerequisites` rows with `prerequisites`
    /// (a `prerequisites` history row when the set changed), and applies
    /// [`Event::Edit`] so the `edit` history row lands in the same transaction.
    ///
    /// On success the old attachment set is deleted and the new one is live.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Rejected`] when the job already left
    /// `requested`, and then nothing changes: neither the store nor the files
    /// on disk. The staged set stays for the caller to discard. Fails with
    /// [`StoreError::InvalidPrerequisite`] when an `after` id would be invalid
    /// for a new job, is the job itself, or would form a cycle, or when a
    /// `keep` row is not one of the job's; nothing changes then either. Fails with
    /// [`StoreError::Io`] on a file system error, and with [`StoreError::Db`]
    /// on a database error; either restores the old set and leaves the job
    /// unchanged.
    pub fn edit_request(
        &self,
        id: i64,
        request: String,
        data_dir: &Path,
        image_names: &[String],
        files: &[JobFile],
        prerequisites: &PrerequisiteEdit<'_>,
    ) -> Result<(), StoreError> {
        let event = Event::Edit { request };
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut row = Self::load(&tx, id)?;
        if *row.job.stage() != Stage::Requested {
            return Err(StoreError::Rejected(Rejected::Unexpected {
                stage: row.job.stage().clone(),
                event: Box::new(event),
            }));
        }
        let change = Self::plan_prerequisites(&tx, id, &row.project, prerequisites)?;
        let job_dir = data_dir.join("jobs").join(id.to_string());
        let swap = DirSwap::swap_in(&job_dir)?;
        let committed = Self::write_edit(&tx, id, &mut row, event, image_names, files, &change)
            .and_then(|()| tx.commit().map_err(StoreError::from));
        match committed {
            Ok(()) => {
                swap.commit();
                Ok(())
            }
            Err(err) => {
                swap.undo();
                Err(err)
            }
        }
    }

    /// Replaces the attachment rows of job `id` and applies `event` to `row`,
    /// without committing, so [`Store::edit_request`] can still roll the disk
    /// swap back if the commit itself fails.
    fn write_edit(
        tx: &Transaction<'_>,
        id: i64,
        row: &mut JobRow,
        event: Event,
        image_names: &[String],
        files: &[JobFile],
        change: &PrerequisiteChange,
    ) -> Result<(), StoreError> {
        tx.execute("DELETE FROM job_images WHERE job_id = ?1", [id])?;
        tx.execute("DELETE FROM job_files WHERE job_id = ?1", [id])?;
        for name in image_names {
            tx.execute(
                "INSERT INTO job_images (job_id, name) VALUES (?1, ?2)",
                params![id, name],
            )?;
        }
        for file in files {
            tx.execute(
                "INSERT INTO job_files (job_id, name, original_name) VALUES (?1, ?2, ?3)",
                params![id, file.name, file.original_name],
            )?;
        }
        if change.changed {
            tx.execute(
                "DELETE FROM job_prerequisites
                 WHERE job_id = ?1 AND id NOT IN (SELECT value FROM json_each(?2))",
                params![id, serde_json::to_string(&change.keep)?],
            )?;
            for prerequisite in &change.added {
                tx.execute(
                    "INSERT INTO job_prerequisites (job_id, prerequisite) VALUES (?1, ?2)",
                    params![id, prerequisite],
                )?;
            }
            let detail = if change.names.is_empty() {
                "none".to_owned()
            } else {
                change
                    .names
                    .iter()
                    .map(|prerequisite| format!("#{prerequisite}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            Self::record(tx, id, "prerequisites", &row.job, Some(&detail), None)?;
        }
        Self::apply_in(tx, row, event)
    }

    /// Checks the posted prerequisites of queued job `id` and works out what
    /// the replacement changes, without writing anything, so a rejection
    /// leaves the files and rows alone.
    fn plan_prerequisites(
        tx: &Transaction<'_>,
        id: i64,
        project: &str,
        edit: &PrerequisiteEdit<'_>,
    ) -> Result<PrerequisiteChange, StoreError> {
        let mut stmt = tx.prepare(
            "SELECT id, prerequisite, gone IS NULL FROM job_prerequisites
             WHERE job_id = ?1 ORDER BY id",
        )?;
        let stored: Vec<(i64, i64, bool)> = stmt
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut keep = edit.keep.to_vec();
        keep.sort_unstable();
        keep.dedup();
        for &row in &keep {
            if !stored.iter().any(|(stored_row, ..)| *stored_row == row) {
                return Err(StoreError::InvalidPrerequisite(row));
            }
        }
        let mut after = edit.after.to_vec();
        after.sort_unstable();
        after.dedup();
        let dependents = Self::dependents_in(tx, id)?;
        let mut added = Vec::new();
        for &prerequisite in &after {
            if prerequisite == id {
                return Err(StoreError::InvalidPrerequisite(prerequisite));
            }
            match Self::load(tx, prerequisite) {
                Ok(row) if row.project == project && row.job.accepts_as_prerequisite() => {}
                Ok(_) | Err(StoreError::NotFound(_)) => {
                    return Err(StoreError::InvalidPrerequisite(prerequisite));
                }
                Err(err) => return Err(err),
            }
            if dependents.contains(&prerequisite) {
                return Err(StoreError::InvalidPrerequisite(prerequisite));
            }
            let existing = stored
                .iter()
                .find(|(_, number, live)| *live && *number == prerequisite);
            match existing {
                Some((row, ..)) => keep.push(*row),
                None => added.push(prerequisite),
            }
        }
        keep.sort_unstable();
        keep.dedup();
        let mut names: Vec<i64> = stored
            .iter()
            .filter(|(row, ..)| keep.contains(row))
            .map(|(_, number, _)| *number)
            .collect();
        names.extend(&added);
        let changed = !added.is_empty() || stored.iter().any(|(row, ..)| !keep.contains(row));
        Ok(PrerequisiteChange {
            keep,
            added,
            names,
            changed,
        })
    }

    /// The jobs that wait for job `id`, directly or through other jobs, over
    /// live prerequisite rows.
    fn dependents_in(conn: &Connection, id: i64) -> Result<HashSet<i64>, StoreError> {
        let mut stmt = conn.prepare(
            "SELECT job_id FROM job_prerequisites WHERE prerequisite = ?1 AND gone IS NULL",
        )?;
        let mut seen = HashSet::new();
        let mut queue = vec![id];
        while let Some(next) = queue.pop() {
            let waiting: Vec<i64> = stmt
                .query_map([next], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for job in waiting {
                if seen.insert(job) {
                    queue.push(job);
                }
            }
        }
        Ok(seen)
    }

    /// The jobs queued job `id` may be told to wait for: the project's
    /// eligible jobs, except `id` itself and the jobs that wait for it, which
    /// would form a cycle.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`, and on
    /// a database error.
    pub fn prerequisite_candidates(&self, id: i64) -> Result<Vec<JobRow>, StoreError> {
        let project = self.get(id)?.project;
        let dependents = Self::dependents_in(&self.conn(), id)?;
        Ok(self
            .list()?
            .into_iter()
            .filter(|row| {
                row.project == project
                    && row.id != id
                    && row.job.can_be_prerequisite()
                    && !dependents.contains(&row.id)
            })
            .collect())
    }

    /// Adds `usd` to the running API cost of job `id` and to the totals of
    /// its project. `estimated` marks the `cost` row as a figure the factory
    /// estimated, such as a Codex run's cost from the price table, rather than
    /// one the agent reported, so the job page can flag a total that includes an
    /// estimate. The amount counts the same in both the job total and the
    /// project totals.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`, and on
    /// a database error.
    pub fn add_cost(&self, id: i64, usd: f64, estimated: bool) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        Self::add_cost_in(&tx, id, usd, estimated)?;
        Ok(tx.commit()?)
    }

    /// Adds the cost of one run to job `id` when the agent reports `total`,
    /// the cost of the whole session whose factory UUID is `uuid` so far.
    /// Claude reports that total, and it keeps counting across `--resume`, so
    /// the run cost the increase over the total the session last reported.
    /// A total lower than the stored one means the agent started the session
    /// over, so the whole total is the run's. Returns the cost added.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`, and on
    /// a database error.
    pub fn add_session_cost(&self, id: i64, uuid: &str, total: f64) -> Result<f64, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let previous: Option<f64> = tx
            .query_row(
                "SELECT cost_usd FROM sessions WHERE uuid = ?1",
                params![uuid],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let usd = match previous {
            Some(previous) if total >= previous => total - previous,
            _ => total,
        };
        tx.execute(
            "UPDATE sessions SET cost_usd = ?2 WHERE uuid = ?1",
            params![uuid, total],
        )?;
        Self::add_cost_in(&tx, id, usd, false)?;
        tx.commit()?;
        Ok(usd)
    }

    fn add_cost_in(
        tx: &rusqlite::Transaction<'_>,
        id: i64,
        usd: f64,
        estimated: bool,
    ) -> Result<(), StoreError> {
        let closed = tx
            .query_row(
                "SELECT 1 FROM cost_closed_jobs WHERE job_id = ?1",
                [id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if closed {
            return Ok(());
        }
        let row = Self::load(tx, id)?;
        Self::record_cost(tx, id, "cost", &row.job, None, Some(usd), estimated)?;
        tx.execute(
            "INSERT INTO job_costs (job_id, usd) VALUES (?1, ?2)
             ON CONFLICT (job_id) DO UPDATE SET usd = usd + excluded.usd",
            params![id, usd],
        )?;
        tx.execute(
            "INSERT INTO project_totals (project, usd) VALUES (?1, ?2)
             ON CONFLICT (project) DO UPDATE SET usd = usd + excluded.usd",
            params![row.project, usd],
        )?;
        Ok(())
    }

    /// Records that MCP `server` was unavailable for job `id`, with `reason`.
    /// It writes a factory line to the job log and an `mcp-unavailable` row to
    /// `job_events` in one transaction, the way [`Store::add_cost`] writes a
    /// `cost` row. Neither changes the job's stage or turn. The row carries the
    /// server and reason in its `detail`, so the timeline shows it as a note
    /// under the step the job was in. The reason names no credential.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`, and on a
    /// database error.
    pub fn record_mcp_unavailable(
        &self,
        id: i64,
        server: &str,
        reason: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row = Self::load(&tx, id)?;
        Self::record(
            &tx,
            id,
            "mcp-unavailable",
            &row.job,
            Some(&format!("{server}: {reason}")),
            None,
        )?;
        tx.execute(
            "INSERT INTO logs (job_id, kind, text) VALUES (?1, ?2, ?3)",
            params![
                id,
                "factory",
                format!(
                    "MCP server {server} is unavailable: {reason}. \
                     The run goes on without its tools."
                )
            ],
        )?;
        Ok(tx.commit()?)
    }

    /// Records that the MCP proxy refused a call to `tool` of `server`, which
    /// the job's project turned off. It writes a factory line to the job log and
    /// an `mcp-tool-blocked` row to `job_events` with `<server>: <tool>` in its
    /// detail. Neither changes the job's stage or turn.
    ///
    /// # Errors
    ///
    /// Fails on a database error or an unknown job.
    pub fn record_mcp_tool_blocked(
        &self,
        id: i64,
        server: &str,
        tool: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row = Self::load(&tx, id)?;
        Self::record(
            &tx,
            id,
            "mcp-tool-blocked",
            &row.job,
            Some(&format!("{server}: {tool}")),
            None,
        )?;
        tx.execute(
            "INSERT INTO logs (job_id, kind, text) VALUES (?1, ?2, ?3)",
            params![
                id,
                "factory",
                format!(
                    "The agent called the MCP tool {tool} of {server}, which this project \
                     turned off. The factory refused the call."
                )
            ],
        )?;
        Ok(tx.commit()?)
    }

    /// Records that the MCP proxy forwarded a call to `tool` of `server` for
    /// job `id`. It writes an `mcp-tool-call` row to `job_events` with
    /// `<server>: <tool>` in its detail. The row carries the job's current
    /// stage, pull request and turn and changes none of them, the way
    /// [`Store::add_cost`] writes a `cost` row. It writes no job log line,
    /// because the agent's own `tool mcp__…` line already records the call, and
    /// it is not deduplicated: every forwarded call is a row.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`, and on a
    /// database error.
    pub fn record_mcp_tool_call(
        &self,
        id: i64,
        server: &str,
        tool: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row = Self::load(&tx, id)?;
        Self::record(
            &tx,
            id,
            "mcp-tool-call",
            &row.job,
            Some(&format!("{server}: {tool}")),
            None,
        )?;
        Ok(tx.commit()?)
    }

    fn record(
        tx: &Transaction<'_>,
        id: i64,
        event: &str,
        job: &Job,
        detail: Option<&str>,
        usd: Option<f64>,
    ) -> Result<(), StoreError> {
        Self::record_cost(tx, id, event, job, detail, usd, false)
    }

    /// The general form of [`Self::record`] that also sets the `estimated` flag,
    /// used for a `cost` row the factory estimated rather than read from the
    /// agent. Every other caller records a figure that is not an estimate.
    fn record_cost(
        tx: &Transaction<'_>,
        id: i64,
        event: &str,
        job: &Job,
        detail: Option<&str>,
        usd: Option<f64>,
        estimated: bool,
    ) -> Result<(), StoreError> {
        let stage = job.stage();
        tx.execute(
            "INSERT INTO job_events (job_id, event, stage, pr, turn, detail, usd, estimated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                event,
                stage.name(),
                stage.pr().map(|pr| i64::try_from(pr).unwrap_or(i64::MAX)),
                job.turn().name(),
                detail,
                usd,
                estimated
            ],
        )?;
        Ok(())
    }

    /// The history of job `id`, oldest first. A job stored before the
    /// history existed has entries only for what happened since.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn events(&self, id: i64) -> Result<Vec<JobEvent>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT at, event, stage, pr, turn, detail, usd, estimated FROM job_events
             WHERE job_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |row| {
            let pr: Option<i64> = row.get(3)?;
            Ok(JobEvent {
                at: row.get(0)?,
                event: row.get(1)?,
                stage: row.get(2)?,
                estimated: row.get(7)?,
                pr: pr.and_then(|pr| usize::try_from(pr).ok()),
                turn: row.get(4)?,
                detail: row.get(5)?,
                usd: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The totals of `project`. A project with no jobs since the totals were
    /// introduced reads 0 everywhere.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_totals(&self, project: &str) -> Result<ProjectTotals, StoreError> {
        let totals = self
            .conn()
            .query_row(
                "SELECT usd, jobs_created, jobs_merged FROM project_totals WHERE project = ?1",
                [project],
                |row| {
                    Ok(ProjectTotals {
                        usd: row.get(0)?,
                        jobs_created: row.get(1)?,
                        jobs_merged: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(totals.unwrap_or(ProjectTotals {
            usd: 0.0,
            jobs_created: 0,
            jobs_merged: 0,
        }))
    }

    /// The API cost of job `id` in USD, summed over its agent runs. A job
    /// with no recorded cost, such as one from before costs were stored,
    /// reads 0.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn cost_usd(&self, id: i64) -> Result<f64, StoreError> {
        Ok(self.recorded_cost(id)?.unwrap_or(0.0))
    }

    /// The API cost of job `id` in USD, or `None` when the job has no cost row.
    /// A zero row counts only for a job that started a Local worker, so it
    /// shows `$0.00`. Any other zero row reads as no cost.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn recorded_cost(&self, id: i64) -> Result<Option<f64>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT usd FROM job_costs c WHERE job_id = ?1 AND {SHOWN_COST}"),
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The API cost in USD of every job that has a `job_costs` row, by job id.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn costs(&self) -> Result<HashMap<i64, f64>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT job_id, usd FROM job_costs c WHERE {SHOWN_COST}"
        ))?;
        let costs = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(costs)
    }

    /// Whether any of job `id`'s cost rows is an estimate, such as a Codex run's
    /// cost from the price table. The job page shows a `≈` on the total when so.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn has_estimated_cost(&self, id: i64) -> Result<bool, StoreError> {
        let found: Option<i64> = self
            .conn()
            .query_row(
                "SELECT 1 FROM job_events
                 WHERE job_id = ?1 AND event = 'cost' AND estimated = 1 LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Every Codex price row, by model id. The table starts empty, so a fresh
    /// install reads none.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn codex_prices(&self) -> Result<Vec<CodexPrice>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT model, input_usd_per_mtok, cached_input_usd_per_mtok, output_usd_per_mtok
             FROM codex_prices ORDER BY model",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CodexPrice {
                model: row.get(0)?,
                input_usd_per_mtok: row.get(1)?,
                cached_input_usd_per_mtok: row.get(2)?,
                output_usd_per_mtok: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The Local agent's settings, the defaults when none are stored.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn local_settings(&self) -> Result<LocalSettings, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT url, cli, load_timeout_minutes, models_fetched_at, models_url
                 FROM local_settings WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map_or_else(
            LocalSettings::default,
            |(url, cli, minutes, fetched, models_url)| LocalSettings {
                url,
                cli: Cli::from_id(&cli).unwrap_or(Cli::Codex),
                load_timeout_minutes: minutes,
                models_fetched_at: fetched,
                models_url,
            },
        ))
    }

    /// Stores the Local server, the CLI and the model load timeout. The URL is
    /// stored as given, so the caller normalizes it first. The stored model list
    /// stays.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Local`] when the timeout is outside 1 to 60
    /// minutes, and on a database error. A refused save writes nothing.
    pub fn save_local_settings(
        &self,
        url: Option<&str>,
        cli: Cli,
        load_timeout_minutes: u32,
    ) -> Result<(), StoreError> {
        if !(1..=MAX_LOCAL_LOAD_MINUTES).contains(&load_timeout_minutes) {
            return Err(StoreError::Local(format!(
                "The model load timeout must be 1 to {MAX_LOCAL_LOAD_MINUTES} minutes."
            )));
        }
        self.conn().execute(
            "INSERT INTO local_settings (id, url, cli, load_timeout_minutes)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET url = ?1, cli = ?2,
                 load_timeout_minutes = ?3, updated_at = CURRENT_TIMESTAMP",
            params![url, cli.id(), load_timeout_minutes],
        )?;
        Ok(())
    }

    /// The model ids the Local server listed at the last successful fetch, in
    /// the server's order.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn local_models(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM local_models ORDER BY position")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replaces the stored model list with `ids` and records `fetched_at`, in
    /// Unix seconds. A failed fetch must not call this, so the earlier list
    /// stays.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn replace_local_models(&self, ids: &[String], fetched_at: i64) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM local_models", [])?;
        for (position, id) in (0_i64..).zip(ids) {
            tx.execute(
                "INSERT OR IGNORE INTO local_models (id, position) VALUES (?1, ?2)",
                params![id, position],
            )?;
        }
        tx.execute(
            "INSERT INTO local_settings (id, models_fetched_at) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET models_fetched_at = ?1, models_url = url",
            [fetched_at],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The Codex price for `model`, or `None` when the table has no row for it.
    /// The executor reads it to estimate a run's cost; with no row the run adds
    /// no cost.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn codex_price(&self, model: &str) -> Result<Option<CodexPrice>, StoreError> {
        let price = self
            .conn()
            .query_row(
                "SELECT model, input_usd_per_mtok, cached_input_usd_per_mtok, output_usd_per_mtok
                 FROM codex_prices WHERE model = ?1",
                [model],
                |row| {
                    Ok(CodexPrice {
                        model: row.get(0)?,
                        input_usd_per_mtok: row.get(1)?,
                        cached_input_usd_per_mtok: row.get(2)?,
                        output_usd_per_mtok: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(price)
    }

    /// Adds `price` or replaces the row for its model, so a save on the settings
    /// page both adds a new model and edits an existing one. The next Codex run
    /// reads the price afresh, so a change applies with no restart.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn set_codex_price(&self, price: &CodexPrice) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO codex_prices
                 (model, input_usd_per_mtok, cached_input_usd_per_mtok, output_usd_per_mtok)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (model) DO UPDATE SET
                 input_usd_per_mtok = excluded.input_usd_per_mtok,
                 cached_input_usd_per_mtok = excluded.cached_input_usd_per_mtok,
                 output_usd_per_mtok = excluded.output_usd_per_mtok",
            params![
                price.model,
                price.input_usd_per_mtok,
                price.cached_input_usd_per_mtok,
                price.output_usd_per_mtok
            ],
        )?;
        Ok(())
    }

    /// Deletes the Codex price for `model`. A model with no row is a no-op.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn delete_codex_price(&self, model: &str) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM codex_prices WHERE model = ?1", [model])?;
        Ok(())
    }

    /// The image file names saved for job `id`, in upload order.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn images(&self, id: i64) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT name FROM job_images WHERE job_id = ?1 ORDER BY rowid")?;
        let rows = stmt.query_map([id], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The files saved for job `id`, in upload order.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn files(&self, id: i64) -> Result<Vec<JobFile>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, original_name FROM job_files WHERE job_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map([id], |row| {
            Ok(JobFile {
                name: row.get(0)?,
                original_name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The stored settings, or the defaults when none are stored.
    ///
    /// # Errors
    ///
    /// Fails on a database error or stored settings that do not parse.
    pub fn settings(&self) -> Result<Settings, StoreError> {
        Self::settings_in(&self.conn())
    }

    fn settings_in(conn: &Connection) -> Result<Settings, StoreError> {
        let stored: Option<String> = conn
            .query_row("SELECT settings FROM settings WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(match stored {
            Some(json) => serde_json::from_str(&json)?,
            None => Settings::default(),
        })
    }

    /// The approval gates a new job of `project` starts with: the project's
    /// saved gates, or the default gates in the settings when it has no row.
    ///
    /// # Errors
    ///
    /// Fails on a database error or stored settings that do not parse.
    pub fn project_gates(&self, project: &str) -> Result<Gates, StoreError> {
        let conn = self.conn();
        let stored = conn
            .query_row(
                "SELECT spec, plan, publish FROM project_gates WHERE project = ?1",
                [project],
                |row| {
                    Ok(Gates {
                        spec: row.get(0)?,
                        plan: row.get(1)?,
                        publish: row.get(2)?,
                    })
                },
            )
            .optional()?;
        match stored {
            Some(gates) => Ok(gates),
            None => Ok(Self::settings_in(&conn)?.gates),
        }
    }

    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_gates(&self, project: &str, gates: Gates) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO project_gates (project, spec, plan, publish) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (project) DO UPDATE SET spec = excluded.spec, plan = excluded.plan,
                publish = excluded.publish",
            params![project, gates.spec, gates.plan, gates.publish],
        )?;
        Ok(())
    }

    fn insert_project_gates(
        conn: &Connection,
        project: &str,
        gates: Gates,
    ) -> Result<(), StoreError> {
        conn.execute(
            "INSERT OR IGNORE INTO project_gates (project, spec, plan, publish)
             VALUES (?1, ?2, ?3, ?4)",
            params![project, gates.spec, gates.plan, gates.publish],
        )?;
        Ok(())
    }

    /// Gives every onboarded project without its own gates a copy of the
    /// default gates in the settings. Gates were one global setting before, so
    /// this keeps each existing project's behaviour. A project that has a row
    /// keeps it, so running this again changes nothing.
    ///
    /// # Errors
    ///
    /// Fails on a database error or stored settings that do not parse.
    pub fn migrate_project_gates(&self) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let gates = Self::settings_in(&tx)?.gates;
        let names = {
            let mut stmt = tx.prepare(
                "SELECT name FROM projects
                 WHERE name NOT IN (SELECT project FROM project_gates)",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for name in names {
            Self::insert_project_gates(&tx, &name, gates)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The collaborators of a project, oldest first, with who added each and
    /// when.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn collaborators(&self, project: &str) -> Result<Vec<Collaborator>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT login, added_by, added_at, use_my_account FROM project_collaborators
             WHERE project = ?1 ORDER BY added_at, login",
        )?;
        let rows = stmt.query_map([project], |row| {
            Ok(Collaborator {
                login: row.get(0)?,
                added_by: row.get(1)?,
                added_at: row.get(2)?,
                use_my_account: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Whether `login` has "Use my account" on for `project`. A login that is
    /// not a collaborator reads as off.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn use_my_account(&self, project: &str, login: &str) -> Result<bool, StoreError> {
        let on = self
            .conn()
            .query_row(
                "SELECT use_my_account FROM project_collaborators WHERE project = ?1 AND login = ?2",
                params![project, login],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        Ok(on.unwrap_or(false))
    }

    /// Sets the "Use my account" switch of the collaborator `login`. A login
    /// that is not a collaborator is left unchanged.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn set_use_my_account(
        &self,
        project: &str,
        login: &str,
        on: bool,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE project_collaborators SET use_my_account = ?3
             WHERE project = ?1 AND login = ?2",
            params![project, login, on],
        )?;
        Ok(())
    }

    /// Whether `login` is a collaborator on `project`.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn is_collaborator(&self, project: &str, login: &str) -> Result<bool, StoreError> {
        let found = self
            .conn()
            .query_row(
                "SELECT 1 FROM project_collaborators WHERE project = ?1 AND login = ?2",
                params![project, login],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    /// The account the forge token of `owner` belongs to, or `None` when no
    /// account is recorded.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn forge_account(
        &self,
        owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<ForgeAccount>, StoreError> {
        read_forge_account(&self.conn(), owner)
    }

    /// The sealed forge token at `at` and the account recorded for `owner`,
    /// read in one transaction, so a concurrent save cannot pair a token with
    /// another token's account. `None` when either is missing.
    ///
    /// # Errors
    ///
    /// Fails on a database error, and with [`StoreError::InstanceChanged`]
    /// when the instance `owner` names is no longer the registration it holds.
    pub fn forge_token_snapshot(
        &self,
        at: SecretRef<'_>,
        owner: ForgeTokenOwner<'_>,
    ) -> Result<Option<(String, Vec<u8>, ForgeAccount)>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        require_registered(&tx, owner.forge())?;
        let sealed = tx
            .query_row(
                "SELECT backend, value FROM secrets
                 WHERE project = ?1 AND server = ?2 AND header = ?3",
                params![at.project, at.server, at.header],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let account = read_forge_account(&tx, owner)?;
        Ok(sealed
            .zip(account)
            .map(|((backend, value), account)| (backend, value, account)))
    }

    /// Records the account the forge token of `owner` belongs to, replacing
    /// the one recorded.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn set_forge_account(
        &self,
        owner: ForgeTokenOwner<'_>,
        account: &ForgeAccount,
    ) -> Result<(), StoreError> {
        write_forge_account(&self.conn(), owner, account)
    }

    /// Forgets the account recorded for the forge token of `owner`.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn clear_forge_account(&self, owner: ForgeTokenOwner<'_>) -> Result<(), StoreError> {
        erase_forge_account(&self.conn(), owner)
    }

    /// Stores the sealed forge token and the account it belongs to in one
    /// transaction, so a failure leaves neither and a concurrent save or clear
    /// never leaves one without the other.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn put_forge_token(
        &self,
        at: SecretRef<'_>,
        backend: &str,
        value: &[u8],
        owner: ForgeTokenOwner<'_>,
        account: &ForgeAccount,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        require_registered(&tx, owner.forge())?;
        tx.execute(
            "INSERT INTO secrets (project, server, header, backend, value)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (project, server, header)
             DO UPDATE SET backend = excluded.backend, value = excluded.value",
            params![at.project, at.server, at.header, backend, value],
        )?;
        write_forge_account(&tx, owner, account)?;
        mark_project_token_handled(&tx, owner)?;
        tx.commit()?;
        Ok(())
    }

    /// The project `name` with the identifier of this instance of it, read
    /// in one transaction. The identifier is never reused, so a project removed
    /// and onboarded again under the name has a different one.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_with_instance(&self, name: &str) -> Result<Option<(Project, i64)>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let project = tx
            .query_row(
                "SELECT forge, repo, base, checks,
                        (SELECT base_url FROM forgejo_instances WHERE name = projects.forge),
                        (SELECT registration FROM forgejo_instances WHERE name = projects.forge)
                 FROM projects WHERE name = ?1",
                [name],
                project_row_without_name,
            )
            .optional()?;
        let Some(project) = project else {
            return Ok(None);
        };
        tx.execute(
            "INSERT OR IGNORE INTO project_instances (project) VALUES (?1)",
            [name],
        )?;
        let instance = tx.query_row(
            "SELECT instance FROM project_instances WHERE project = ?1",
            [name],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some((project, instance)))
    }

    /// As [`Store::put_forge_token`] for a project token, and writes nothing
    /// unless the project instance in `guard` still exists and `guard.login` is
    /// still its collaborator. Returns whether it wrote.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn put_project_forge_token(
        &self,
        at: SecretRef<'_>,
        backend: &str,
        value: &[u8],
        owner: ForgeTokenOwner<'_>,
        account: &ForgeAccount,
        guard: &ProjectGuard<'_>,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let present: bool = tx.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM project_instances i
                 JOIN project_collaborators c ON c.project = i.project
                 WHERE i.project = ?1 AND i.instance = ?2 AND c.login = ?3
             )",
            params![guard.project, guard.instance, guard.login],
            |row| row.get(0),
        )?;
        if !present {
            return Ok(false);
        }
        require_registered(&tx, owner.forge())?;
        tx.execute(
            "INSERT INTO secrets (project, server, header, backend, value)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (project, server, header)
             DO UPDATE SET backend = excluded.backend, value = excluded.value",
            params![at.project, at.server, at.header, backend, value],
        )?;
        write_forge_account(&tx, owner, account)?;
        mark_project_token_handled(&tx, owner)?;
        tx.commit()?;
        Ok(true)
    }

    /// Removes the sealed forge token and its account in one transaction.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn delete_forge_token(
        &self,
        at: SecretRef<'_>,
        owner: ForgeTokenOwner<'_>,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let ForgeTokenOwner::Upstream { project, forge } = owner {
            // A project keeps one upstream token, so clearing one forge's
            // leaves a token stored for the other forge alone.
            let stored: Option<String> = tx
                .query_row(
                    "SELECT forge FROM project_forge_accounts WHERE project = ?1 AND kind = 'upstream'",
                    [project],
                    |row| row.get(0),
                )
                .optional()?;
            if stored.is_some_and(|stored| stored != forge.as_str()) {
                return Ok(());
            }
        }
        tx.execute(
            "DELETE FROM secrets WHERE project = ?1 AND server = ?2 AND header = ?3",
            params![at.project, at.server, at.header],
        )?;
        erase_forge_account(&tx, owner)?;
        mark_project_token_handled(&tx, owner)?;
        tx.commit()?;
        Ok(())
    }

    /// Whether `login` wants browser notifications. A login with no
    /// `user_settings` row has them on.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn notifications_enabled(&self, login: &str) -> Result<bool, StoreError> {
        let stored = self
            .conn()
            .query_row(
                "SELECT notifications FROM user_settings WHERE login = ?1",
                [login],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        Ok(stored.unwrap_or(true))
    }

    /// Saves whether `login` wants browser notifications. It writes that
    /// login's row alone.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn set_notifications(&self, login: &str, enabled: bool) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO user_settings (login, notifications) VALUES (?1, ?2)
             ON CONFLICT (login) DO UPDATE SET
                 notifications = excluded.notifications,
                 updated_at = CURRENT_TIMESTAMP",
            params![login, enabled],
        )?;
        Ok(())
    }

    /// The project groups `login` made, in name order, each with the
    /// identifiers of its projects. A project removed from the factory has
    /// no membership left; one the login stopped collaborating on keeps its
    /// row, and the caller leaves it out by listing only the login's projects.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_groups(&self, login: &str) -> Result<Vec<ProjectGroup>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name FROM project_groups WHERE login = ?1
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let mut groups = stmt
            .query_map([login], |row| {
                Ok(ProjectGroup {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    projects: Vec::new(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT group_id, project FROM project_group_members WHERE login = ?1
             ORDER BY project",
        )?;
        let members = stmt.query_map([login], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for member in members {
            let (id, project) = member?;
            if let Some(group) = groups.iter_mut().find(|group| group.id == id) {
                group.projects.push(project);
            }
        }
        Ok(groups)
    }

    /// Renames `login`'s groups, removes the ones whose new name is empty
    /// with their memberships, and adds `added` when it is set. An id that is
    /// not one of the login's groups changes nothing. The caller checks the
    /// names are unique.
    ///
    /// # Errors
    ///
    /// Fails on a database error. Nothing is written when it fails.
    pub fn save_project_groups(
        &self,
        login: &str,
        names: &[(i64, String)],
        added: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (id, name) in names {
            if name.is_empty() {
                tx.execute(
                    "DELETE FROM project_group_members WHERE login = ?1 AND group_id = ?2",
                    params![login, id],
                )?;
                tx.execute(
                    "DELETE FROM project_groups WHERE login = ?1 AND id = ?2",
                    params![login, id],
                )?;
            } else {
                tx.execute(
                    "UPDATE project_groups SET name = ?3 WHERE login = ?1 AND id = ?2",
                    params![login, id, name],
                )?;
            }
        }
        if let Some(name) = added {
            tx.execute(
                "INSERT INTO project_groups (login, name) VALUES (?1, ?2)",
                params![login, name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Puts each project of `assignments` in the group it names, or in no
    /// group for `None`. A group id that is not one of `login`'s groups puts
    /// the project in no group.
    ///
    /// # Errors
    ///
    /// Fails on a database error. Nothing is written when it fails.
    pub fn set_project_groups(
        &self,
        login: &str,
        assignments: &[(String, Option<i64>)],
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (project, group) in assignments {
            tx.execute(
                "DELETE FROM project_group_members WHERE login = ?1 AND project = ?2",
                params![login, project],
            )?;
            if let Some(group) = group {
                tx.execute(
                    "INSERT INTO project_group_members (login, project, group_id)
                     SELECT ?1, ?2, id FROM project_groups WHERE login = ?1 AND id = ?3",
                    params![login, project, group],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The names of the projects `login` collaborates on, in name order.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn projects_for(&self, login: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT project FROM project_collaborators
             WHERE login = ?1 ORDER BY project",
        )?;
        let rows = stmt.query_map([login], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The projects `login` collaborates on, with the name pages show for
    /// each, in shown-name order.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_labels_for(&self, login: &str) -> Result<Vec<ProjectLabel>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.project, COALESCE(p.display_name, c.project)
             FROM project_collaborators c LEFT JOIN projects p ON p.name = c.project
             WHERE c.login = ?1 ORDER BY 2, 1",
        )?;
        let rows = stmt.query_map([login], |row| {
            Ok(ProjectLabel {
                id: row.get(0)?,
                shown: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The name pages show for `project`: its display name, else `project`.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_shown_name(&self, project: &str) -> Result<String, StoreError> {
        let display: Option<Option<String>> = self
            .conn()
            .query_row(
                "SELECT display_name FROM projects WHERE name = ?1",
                [project],
                |row| row.get(0),
            )
            .optional()?;
        Ok(display.flatten().unwrap_or_else(|| project.to_owned()))
    }

    /// Whether `name` is the identifier or the display name of any project.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn name_in_use(&self, name: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM projects WHERE name = ?1 OR display_name = ?1",
                [name],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Sets the display name of `project`, or clears it with `None`.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::DuplicateName`] when another project has that
    /// identifier or display name, and on a database error.
    pub fn save_project_display_name(
        &self,
        project: &str,
        display_name: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(value) = display_name {
            let taken = tx
                .query_row(
                    "SELECT 1 FROM projects
                     WHERE name != ?1 AND (name = ?2 OR display_name = ?2)",
                    params![project, value],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if taken {
                return Err(StoreError::DuplicateName(value.to_owned()));
            }
        }
        tx.execute(
            "UPDATE projects SET display_name = ?2 WHERE name = ?1",
            params![project, display_name],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Adds `login` as a collaborator on `project`, added by `by`. A login that
    /// is already a collaborator is left unchanged.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn add_collaborator(&self, project: &str, login: &str, by: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO project_collaborators (project, login, added_by)
             VALUES (?1, ?2, ?3)",
            params![project, login, by],
        )?;
        Ok(())
    }

    /// Removes `login` from `project`'s collaborators.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::LastCollaborator`] when `login` is the sole
    /// remaining collaborator, since a project with no collaborator is visible
    /// to nobody. Removing a login that is not a collaborator changes nothing.
    /// Also fails on a database error.
    pub fn remove_collaborator(&self, project: &str, login: &str) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM project_collaborators WHERE project = ?1",
            [project],
            |row| row.get(0),
        )?;
        let is_collaborator = tx
            .query_row(
                "SELECT 1 FROM project_collaborators WHERE project = ?1 AND login = ?2",
                params![project, login],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if is_collaborator && count <= 1 {
            return Err(StoreError::LastCollaborator);
        }
        tx.execute(
            "DELETE FROM project_collaborators WHERE project = ?1 AND login = ?2",
            params![project, login],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Gives every project that has no collaborator rows one row for each
    /// distinct `requested_by` login in its jobs, added by `migration`.
    /// Collaborators did not exist before, so this seeds each project from the
    /// logins that already requested jobs in it. A project that already has a
    /// row is skipped, so a later edit survives a restart, and a project with
    /// no jobs gets no rows, which the log names so the admin can add one.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn migrate_project_collaborators(&self) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let names = {
            let mut stmt = tx.prepare(
                "SELECT name FROM projects
                 WHERE name NOT IN (SELECT project FROM project_collaborators)",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for name in names {
            let logins = {
                let mut stmt =
                    tx.prepare("SELECT DISTINCT requested_by FROM jobs WHERE project = ?1")?;
                let rows = stmt.query_map([&name], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            if logins.is_empty() {
                tracing::warn!(
                    "project {name:?} has no jobs, so the collaborator migration left it \
                     with no collaborators; it is visible to nobody until one is added"
                );
                continue;
            }
            for login in logins {
                tx.execute(
                    "INSERT OR IGNORE INTO project_collaborators (project, login, added_by)
                     VALUES (?1, ?2, 'migration')",
                    params![name, login],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The ids of the jobs of `project`, oldest first. These stay after the
    /// project is removed, until job expiry deletes them, so re-onboarding the
    /// name reads them to clear the leftover jobs first.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn job_ids_for(&self, project: &str) -> Result<Vec<i64>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM jobs WHERE project = ?1 ORDER BY id")?;
        let rows = stmt.query_map([project], |row| row.get::<_, i64>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Removes project `name` in one transaction: cancels its unfinished jobs,
    /// and deletes its configuration, its collaborators, its stored MCP header
    /// values and the cost history of its jobs.
    ///
    /// Each job that is not already merged, failed or cancelled gets
    /// [`Event::Cancel`] applied and a factory log line, exactly as the Cancel
    /// button does, so once the caller wakes the scheduler, `stop()` ends any
    /// container a cancelled job was running. The job rows, their logs and their
    /// directories stay until the 30-day expiry removes them, see
    /// [`Self::delete_job`], and open pull requests are left alone.
    ///
    /// The deleted configuration is the project's rows in `projects`,
    /// `project_models`, `project_settings`, `project_gates`, `project_efforts`,
    /// `project_agents`, `project_renovate_agent`, `project_mcp`, `project_mcp_tools`,
    /// `project_mcp_tool_refreshes`, `project_mcp_args`, `project_packages`, `project_totals` and
    /// `project_collaborators`, `project_maintenance`, `renovate_prs`, its MCP header values in `secrets`, its
    /// `job_costs` rows, and the `cost` rows in `job_events` for its jobs. With
    /// no collaborator rows left, the project and its surviving jobs are visible
    /// to nobody. A run that finishes after the removal cannot record its cost:
    /// [`Self::add_cost`] and [`Self::add_session_cost`] skip a job of a removed
    /// project.
    ///
    /// # Errors
    ///
    /// Fails on a database error. Nothing is written when it fails.
    pub fn remove_project(&self, name: &str) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM jobs WHERE project = ?1 ORDER BY id")?;
            let rows = stmt.query_map([name], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for id in &ids {
            let mut row = Self::load(&tx, *id)?;
            if !row.job.is_finished() {
                Self::apply_in(&tx, &mut row, Event::Cancel)?;
                tx.execute(
                    "INSERT INTO logs (job_id, kind, text) VALUES (?1, 'factory', ?2)",
                    params![id, "Cancelled because the project was removed."],
                )?;
            }
            tx.execute("DELETE FROM job_costs WHERE job_id = ?1", [id])?;
            tx.execute(
                "DELETE FROM job_events WHERE job_id = ?1 AND event = 'cost'",
                [id],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO cost_closed_jobs (job_id) VALUES (?1)",
                [id],
            )?;
        }
        for table in [
            "project_models",
            "project_settings",
            "project_gates",
            "project_efforts",
            "project_agents",
            "project_renovate_agent",
            "project_mcp",
            "project_mcp_tools",
            "project_mcp_tool_refreshes",
            "project_mcp_args",
            "project_packages",
            "project_totals",
            "project_collaborators",
            "project_maintenance",
            "renovate_prs",
            "dismissed_gaps",
            "project_forge_accounts",
            "gh_token_copies",
            "project_instances",
            "project_group_members",
            "secrets",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE project = ?1"), [name])?;
        }
        tx.execute("DELETE FROM projects WHERE name = ?1", [name])?;
        tx.commit()?;
        Ok(())
    }

    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_settings(&self, settings: &Settings) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO settings (id, settings) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET settings = excluded.settings",
            [serde_json::to_string(settings)?],
        )?;
        Ok(())
    }

    /// The models saved for `project`, or `defaults` when none are saved.
    /// Saved models replace the defaults as a whole, so saving an empty
    /// stage clears a model the project file sets.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_models(
        &self,
        project: &str,
        defaults: &StageModels,
    ) -> Result<StageModels, StoreError> {
        let stored = self
            .conn()
            .query_row(
                "SELECT clarify, spec, plan, implement, review FROM project_models WHERE project = ?1",
                [project],
                |row| {
                    Ok(StageModels {
                        clarify: row.get(0)?,
                        spec: row.get(1)?,
                        plan: row.get(2)?,
                        implement: row.get(3)?,
                        review: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(stored.unwrap_or_else(|| defaults.clone()))
    }

    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_models(
        &self,
        project: &str,
        models: &StageModels,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO project_models (project, clarify, spec, plan, implement, review)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (project) DO UPDATE SET clarify = excluded.clarify,
                spec = excluded.spec, plan = excluded.plan,
                implement = excluded.implement, review = excluded.review",
            params![
                project,
                models.clarify,
                models.spec,
                models.plan,
                models.implement,
                models.review
            ],
        )?;
        Ok(())
    }

    /// The effort levels saved for `project`, or the default agent's default
    /// effort for every stage when no row is saved, see
    /// [`StageEfforts::default`]. A project in a database made before this table
    /// existed has no row and so reads every stage at that default.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_efforts(&self, project: &str) -> Result<StageEfforts, StoreError> {
        let stored = self
            .conn()
            .query_row(
                "SELECT clarify, spec, plan, implement, review FROM project_efforts WHERE project = ?1",
                [project],
                |row| {
                    Ok(StageEfforts {
                        clarify: row.get(0)?,
                        spec: row.get(1)?,
                        plan: row.get(2)?,
                        implement: row.get(3)?,
                        review: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(stored.unwrap_or_default())
    }

    /// Checks that each stage's effort is a level the stage's agent accepts,
    /// without writing. The save handler calls this before it writes the agents,
    /// efforts and models, so a rejected level leaves all three unchanged. The
    /// agents are those saved in the same submit, so the level is validated
    /// against the agent it will run on.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidAgent`] for the first stage whose agent id
    /// matches no agent, or [`StoreError::InvalidEffort`] for the first stage
    /// whose level the stage's agent does not accept.
    pub fn check_efforts(efforts: &StageEfforts, agents: &StageAgents) -> Result<(), StoreError> {
        for (level, agent_id) in [
            (&efforts.clarify, &agents.clarify),
            (&efforts.spec, &agents.spec),
            (&efforts.plan, &agents.plan),
            (&efforts.implement, &agents.implement),
            (&efforts.review, &agents.review),
        ] {
            let agent =
                Agent::from_id(agent_id).map_err(|_| StoreError::InvalidAgent(agent_id.clone()))?;
            if !agent.accepts_effort(level) {
                return Err(StoreError::InvalidEffort(level.clone()));
            }
        }
        Ok(())
    }

    /// Checks that every stage on the Local agent has a model, without writing,
    /// because the local server picks the model from the id in the request. The
    /// save handler calls this before any write, like [`Store::check_efforts`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingLocalModel`] for the first stage on Local
    /// whose model is empty.
    pub fn check_local_models(
        models: &StageModels,
        agents: &StageAgents,
    ) -> Result<(), StoreError> {
        for (stage, model, agent) in [
            ("clarify", &models.clarify, &agents.clarify),
            ("spec", &models.spec, &agents.spec),
            ("plan", &models.plan, &agents.plan),
            ("implement", &models.implement, &agents.implement),
            ("review", &models.review, &agents.review),
        ] {
            if agent == Agent::Local.id()
                && model.as_deref().is_none_or(|model| model.trim().is_empty())
            {
                return Err(StoreError::MissingLocalModel(stage));
            }
        }
        Ok(())
    }

    /// Saves `project`'s effort for all five stages, writing only this
    /// project's row. Each level must be one its stage's agent accepts, see
    /// [`Store::check_efforts`]; a value outside the agent's set is rejected and
    /// nothing is written.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidAgent`] or [`StoreError::InvalidEffort`] for
    /// a level a stage's agent does not accept, before any write. Otherwise
    /// fails only on a database error.
    pub fn save_project_efforts(
        &self,
        project: &str,
        efforts: &StageEfforts,
        agents: &StageAgents,
    ) -> Result<(), StoreError> {
        Self::check_efforts(efforts, agents)?;
        self.conn().execute(
            "INSERT INTO project_efforts (project, clarify, spec, plan, implement, review)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (project) DO UPDATE SET clarify = excluded.clarify,
                spec = excluded.spec, plan = excluded.plan,
                implement = excluded.implement, review = excluded.review",
            params![
                project,
                efforts.clarify,
                efforts.spec,
                efforts.plan,
                efforts.implement,
                efforts.review
            ],
        )?;
        Ok(())
    }

    /// The agents saved for `project`, or [`DEFAULT_AGENT`] for every stage when
    /// no row is saved. A project in a database made before this table existed
    /// has no row and so reads the default agent for every stage.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_agents(&self, project: &str) -> Result<StageAgents, StoreError> {
        let stored = self
            .conn()
            .query_row(
                "SELECT clarify, spec, plan, implement, review FROM project_agents WHERE project = ?1",
                [project],
                |row| {
                    Ok(StageAgents {
                        clarify: row.get(0)?,
                        spec: row.get(1)?,
                        plan: row.get(2)?,
                        implement: row.get(3)?,
                        review: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(stored.unwrap_or_default())
    }

    /// Checks that every stage names an available agent, without writing. The
    /// save handler calls this before it writes the efforts and models, so a
    /// rejected agent leaves all three unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidAgent`] for the first stage whose id matches
    /// no available agent.
    pub fn check_agents(agents: &StageAgents) -> Result<(), StoreError> {
        for id in [
            &agents.clarify,
            &agents.spec,
            &agents.plan,
            &agents.implement,
            &agents.review,
        ] {
            match Agent::from_id(id) {
                Ok(agent) if agent.available() => {}
                _ => return Err(StoreError::InvalidAgent(id.clone())),
            }
        }
        Ok(())
    }

    /// Saves `project`'s agent for all five stages, writing only this project's
    /// row. Every id must be one an available agent carries, see
    /// [`Store::check_agents`]; an unknown id, or one of an agent that cannot
    /// run, is rejected and nothing is written.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidAgent`] for an id outside the available
    /// agents, before any write. Otherwise fails only on a database error.
    pub fn save_project_agents(
        &self,
        project: &str,
        agents: &StageAgents,
    ) -> Result<(), StoreError> {
        Self::check_agents(agents)?;
        self.conn().execute(
            "INSERT INTO project_agents (project, clarify, spec, plan, implement, review)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (project) DO UPDATE SET clarify = excluded.clarify,
                spec = excluded.spec, plan = excluded.plan,
                implement = excluded.implement, review = excluded.review",
            params![
                project,
                agents.clarify,
                agents.spec,
                agents.plan,
                agents.implement,
                agents.review
            ],
        )?;
        Ok(())
    }

    /// The Renovate agent override saved for `project`. `None` means the
    /// override is off and Renovate runs use the review stage's settings.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_renovate_agent(
        &self,
        project: &str,
    ) -> Result<Option<RenovateAgent>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT agent, model, effort FROM project_renovate_agent WHERE project = ?1",
                [project],
                |row| {
                    Ok(RenovateAgent {
                        agent: row.get(0)?,
                        model: row.get(1)?,
                        effort: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Checks a Renovate agent override with the stage rules, without writing:
    /// the agent is available, accepts the effort, and Local has a model.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidRenovateAgent`] with the reason.
    pub fn check_renovate_agent(agent: &RenovateAgent) -> Result<(), StoreError> {
        let invalid = |reason: String| StoreError::InvalidRenovateAgent(reason);
        let parsed = match Agent::from_id(&agent.agent) {
            Ok(parsed) if parsed.available() => parsed,
            _ => {
                return Err(invalid(format!(
                    "{:?} is not an available agent",
                    agent.agent
                )));
            }
        };
        if !parsed.accepts_effort(&agent.effort) {
            return Err(invalid(format!(
                "{:?} is not an effort level of {}",
                agent.effort, agent.agent
            )));
        }
        if agent.agent == Agent::Local.id()
            && agent
                .model
                .as_deref()
                .is_none_or(|model| model.trim().is_empty())
        {
            return Err(invalid(
                "Local needs a model, because the local server picks the model from its id"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Saves `project`'s Renovate agent override, or removes it with `None`.
    /// A blank model is stored as none. Writes only the override's table.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidRenovateAgent`] before any write, see
    /// [`Store::check_renovate_agent`]. Otherwise fails only on a database
    /// error.
    pub fn save_project_renovate_agent(
        &self,
        project: &str,
        agent: Option<&RenovateAgent>,
    ) -> Result<(), StoreError> {
        match agent {
            Some(agent) => {
                Self::check_renovate_agent(agent)?;
                let model = agent.model.as_deref().filter(|m| !m.trim().is_empty());
                self.conn().execute(
                    "INSERT INTO project_renovate_agent (project, agent, model, effort)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (project) DO UPDATE SET agent = excluded.agent,
                        model = excluded.model, effort = excluded.effort",
                    params![project, agent.agent, model, agent.effort],
                )?;
            }
            None => {
                self.conn().execute(
                    "DELETE FROM project_renovate_agent WHERE project = ?1",
                    [project],
                )?;
            }
        }
        Ok(())
    }

    /// Whether `project` auto-merges its pull requests, or `default` when no
    /// row is saved. A saved row replaces the project file default.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_auto_merge(&self, project: &str, default: bool) -> Result<bool, StoreError> {
        let stored = self
            .conn()
            .query_row(
                "SELECT auto_merge FROM project_settings WHERE project = ?1",
                [project],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.unwrap_or(default))
    }

    /// The saved auto-merge setting of every project `login` collaborates on,
    /// in one read. A project without a saved row maps to `None`, and the
    /// caller applies the project file default as [`Store::project_auto_merge`]
    /// does.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_auto_merges(
        &self,
        login: &str,
    ) -> Result<HashMap<String, Option<bool>>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.project, s.auto_merge
             FROM project_collaborators c
             LEFT JOIN project_settings s ON s.project = c.project
             WHERE c.login = ?1",
        )?;
        let rows = stmt.query_map([login], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
    }

    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_auto_merge(
        &self,
        project: &str,
        auto_merge: bool,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO project_settings (project, auto_merge) VALUES (?1, ?2)
             ON CONFLICT (project) DO UPDATE SET auto_merge = excluded.auto_merge",
            params![project, auto_merge],
        )?;
        Ok(())
    }

    /// Whether `project` runs an LLM review before merge, or `default` when no
    /// row is saved. A saved row replaces the project file default.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_llm_review(&self, project: &str, default: bool) -> Result<bool, StoreError> {
        let stored = self
            .conn()
            .query_row(
                "SELECT llm_review FROM project_settings WHERE project = ?1",
                [project],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.unwrap_or(default))
    }

    /// The maintenance settings of `project`: both switches off when it has
    /// no row.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored list that does not parse.
    pub fn project_maintenance(&self, project: &str) -> Result<Maintenance, StoreError> {
        match self.read_maintenance(project)? {
            (_, Some(name)) => Err(StoreError::UnknownUpstreamForge(name)),
            (maintenance, None) => Ok(maintenance),
        }
    }

    /// As [`Store::project_maintenance`], for a page that must stay editable
    /// when the stored upstream forge is unknown. The settings then read as
    /// the GitHub upstream, and the name of the unknown forge comes back
    /// beside them.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored list that does not parse.
    pub fn project_maintenance_lenient(
        &self,
        project: &str,
    ) -> Result<(Maintenance, Option<String>), StoreError> {
        self.read_maintenance(project)
    }

    fn read_maintenance(&self, project: &str) -> Result<(Maintenance, Option<String>), StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT renovate, packaging, packaging_type, manifest_files,
                        build_config_files, upstream_repo, renovate_author,
                        COALESCE(upstream_forge,
                                 (SELECT forge FROM projects WHERE name = ?1)),
                        (SELECT base_url FROM forgejo_instances
                         WHERE name = COALESCE(upstream_forge,
                                 (SELECT forge FROM projects WHERE name = ?1))),
                        (SELECT registration FROM forgejo_instances
                         WHERE name = COALESCE(upstream_forge,
                                 (SELECT forge FROM projects WHERE name = ?1))),
                        upstream_forge IS NOT NULL
                 FROM project_maintenance WHERE project = ?1",
                [project],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        registered(row, 8)?,
                        row.get::<_, bool>(10)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            renovate,
            packaging,
            kind,
            manifest,
            build,
            upstream,
            renovate_author,
            upstream_forge,
            upstream_instance,
            upstream_chosen,
        )) = row
        else {
            return Ok((Maintenance::default(), None));
        };
        let mut unknown = None;
        let packaging = if packaging {
            Some(Packaging {
                kind: kind
                    .as_deref()
                    .and_then(PackagingType::parse)
                    .unwrap_or_default(),
                manifest_files: serde_json::from_str(&manifest)?,
                build_config_files: serde_json::from_str(&build)?,
                upstream_repo: upstream.unwrap_or_default(),
                // A project that chose no upstream forge reads as its own, and
                // when that is unknown its jobs fail on that, so GitHub here
                // only keeps its settings page readable.
                upstream_forge: match upstream_forge.as_deref() {
                    None => ForgeKind::GitHub,
                    Some(name) => ForgeKind::resolve(
                        name,
                        upstream_instance
                            .as_ref()
                            .map(|(url, id)| (url.as_str(), *id)),
                    )
                    .unwrap_or_else(|| {
                        unknown = upstream_chosen.then(|| name.to_owned());
                        ForgeKind::GitHub
                    }),
                },
            })
        } else {
            None
        };
        Ok((
            Maintenance {
                renovate,
                renovate_author,
                packaging,
            },
            unknown,
        ))
    }

    /// Writes the whole maintenance row of `project`. It does not validate;
    /// call [`Maintenance::check`] first.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_maintenance(
        &self,
        project: &str,
        maintenance: &Maintenance,
    ) -> Result<(), StoreError> {
        write_maintenance(&self.conn(), project, maintenance)
    }

    /// As [`Store::save_project_maintenance`], and writes only while the
    /// project instance and collaborator in `guard` still exist and the sealed
    /// token stored for each owner in `credentials` is still those bytes, or
    /// is still absent for `None`. A token saved again is sealed anew, so it
    /// differs even when it is the same string.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_maintenance_guarded(
        &self,
        guard: &ProjectGuard<'_>,
        maintenance: &Maintenance,
        credentials: &[(ForgeTokenOwner<'_>, Option<&[u8]>)],
    ) -> Result<MaintenanceSave, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let present: bool = tx.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM project_instances i
                 JOIN project_collaborators c ON c.project = i.project
                 WHERE i.project = ?1 AND i.instance = ?2 AND c.login = ?3
             )",
            params![guard.project, guard.instance, guard.login],
            |row| row.get(0),
        )?;
        if !present {
            return Ok(MaintenanceSave::Gone);
        }
        let upstream = maintenance.packaging.as_ref().map(|p| &p.upstream_forge);
        let forges = credentials
            .iter()
            .map(|(owner, _)| owner.forge())
            .chain(upstream);
        for forge in forges {
            match require_registered(&tx, forge) {
                Ok(()) => {}
                Err(StoreError::InstanceChanged(_)) => return Ok(MaintenanceSave::InstanceChanged),
                Err(err) => return Err(err),
            }
        }
        for (owner, sealed) in credentials {
            let (project, header) = owner.row();
            // Account resolution reads a token only when its account row is
            // recorded for this forge, so a ciphertext sealed for another
            // forge counts as absent here too.
            let current: Option<Vec<u8>> = if read_forge_account(&tx, *owner)?.is_some() {
                tx.query_row(
                    "SELECT value FROM secrets
                     WHERE project = ?1 AND server = '' AND header = ?2",
                    params![project, header],
                    |row| row.get(0),
                )
                .optional()?
            } else {
                None
            };
            if current.as_deref() != *sealed {
                return Ok(MaintenanceSave::TokenChanged);
            }
        }
        write_maintenance(&tx, guard.project, maintenance)?;
        tx.commit()?;
        Ok(MaintenanceSave::Saved)
    }

    /// Sets the local checks command of `project`. `None` means no checks. It
    /// does not trim; the caller does.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_checks(
        &self,
        project: &str,
        checks: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE projects SET checks = ?2 WHERE name = ?1",
            params![project, checks],
        )?;
        Ok(())
    }

    /// The nixpkgs packages `project` puts on `PATH` in its workers, in the
    /// order saved. Empty when none is saved.
    ///
    /// # Errors
    ///
    /// Fails on a database error or when the stored list is not valid JSON.
    pub fn project_packages(&self, project: &str) -> Result<Vec<String>, StoreError> {
        let stored: Option<String> = self
            .conn()
            .query_row(
                "SELECT packages FROM project_packages WHERE project = ?1",
                [project],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(Vec::new()),
        }
    }

    /// Saves `project`'s worker packages, writing only this row. The caller has
    /// validated them with [`crate::packages::validate`].
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_packages(
        &self,
        project: &str,
        packages: &[String],
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO project_packages (project, packages) VALUES (?1, ?2)
             ON CONFLICT (project) DO UPDATE SET packages = excluded.packages",
            params![project, serde_json::to_string(packages)?],
        )?;
        Ok(())
    }

    /// Saves a project's auto-merge and LLM-review flags together, so the one
    /// settings form writes the whole row. A saved row replaces the project
    /// file defaults as a whole.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_settings(
        &self,
        project: &str,
        auto_merge: bool,
        llm_review: bool,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO project_settings (project, auto_merge, llm_review)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (project) DO UPDATE SET auto_merge = excluded.auto_merge,
                llm_review = excluded.llm_review",
            params![project, auto_merge, llm_review],
        )?;
        Ok(())
    }

    /// Moves the old global auto-merge setting into each project's own
    /// setting. When the stored settings still carry a global `auto_merge` and
    /// it was on, every `project` listed that has no saved flag of its own
    /// inherits it, so those projects keep today's behaviour. The global field
    /// is then dropped from the stored settings. This runs once: afterwards the
    /// stored settings no longer carry the field, so a later call does nothing.
    ///
    /// # Errors
    ///
    /// Fails on a database error or stored settings that do not parse.
    pub fn migrate_global_auto_merge(&self, projects: &[&str]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let raw: Option<String> = tx
            .query_row("SELECT settings FROM settings WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(raw) = raw else {
            return Ok(());
        };
        let mut value: serde_json::Value = serde_json::from_str(&raw)?;
        let Some(global) = value
            .as_object_mut()
            .and_then(|object| object.remove("auto_merge"))
        else {
            return Ok(());
        };
        if global.as_bool() == Some(true) {
            for project in projects {
                tx.execute(
                    "INSERT OR IGNORE INTO project_settings (project, auto_merge) VALUES (?1, 1)",
                    [project],
                )?;
            }
        }
        tx.execute(
            "UPDATE settings SET settings = ?1 WHERE id = 1",
            [serde_json::to_string(&value)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The agent session UUID for `session` of job `job_id` and the agent the
    /// session runs on, created on first use. A fresh session records
    /// `stage_agent`, the agent the stage runs today. A session that already
    /// exists keeps the agent it started with, even when the stage's agent was
    /// changed since, so a resumed session stays on the agent that began it.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn session_uuid(
        &self,
        job_id: i64,
        session: Session,
        stage_agent: Agent,
    ) -> Result<(String, Agent), StoreError> {
        let key = session_key(session);
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (job_id, session, uuid, agent) VALUES (?1, ?2, ?3, ?4)",
            params![
                job_id,
                key,
                uuid::Uuid::new_v4().to_string(),
                stage_agent.id()
            ],
        )?;
        let (uuid, agent_id): (String, String) = conn.query_row(
            "SELECT uuid, agent FROM sessions WHERE job_id = ?1 AND session = ?2",
            params![job_id, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        // A stored id always matches an agent, since only an agent's own id is
        // ever written; an unreadable one falls back to the default agent rather
        // than failing the run.
        let agent = Agent::from_id(&agent_id).unwrap_or(DEFAULT_AGENT);
        Ok((uuid, agent))
    }

    /// The agent and the model and effort `session` of job `job_id` last ran
    /// with, or `None` when the session has no row yet. The model and effort
    /// read `None` for a row from before they were recorded.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn session_settings(
        &self,
        job_id: i64,
        session: Session,
    ) -> Result<Option<SessionSettings>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT agent, model, effort FROM sessions WHERE job_id = ?1 AND session = ?2",
                params![job_id, session_key(session)],
                |row| {
                    let agent_id: String = row.get(0)?;
                    Ok(SessionSettings {
                        agent: Agent::from_id(&agent_id).unwrap_or(DEFAULT_AGENT),
                        model: row.get(1)?,
                        effort: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// The CLI `session` of job `job_id` started on when it runs on the Local
    /// agent, or `None` for a session that has not recorded one.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn session_local_cli(
        &self,
        job_id: i64,
        session: Session,
    ) -> Result<Option<Cli>, StoreError> {
        let id: Option<String> = self
            .conn()
            .query_row(
                "SELECT local_cli FROM sessions WHERE job_id = ?1 AND session = ?2",
                params![job_id, session_key(session)],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(id.and_then(|id| Cli::from_id(&id)))
    }

    /// Records `cli` as the CLI `session` of job `job_id` started on. A session
    /// that already recorded one keeps it, so only the first Local run of a
    /// session decides.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn record_session_local_cli(
        &self,
        job_id: i64,
        session: Session,
        cli: Cli,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET local_cli = ?3
             WHERE job_id = ?1 AND session = ?2 AND local_cli IS NULL",
            params![job_id, session_key(session), cli.id()],
        )?;
        Ok(())
    }

    /// Records the model and effort a run of `session` of job `job_id` starts
    /// with, for the runs that resume it after the stage's agent changes.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn record_session_settings(
        &self,
        job_id: i64,
        session: Session,
        model: Option<&str>,
        effort: &str,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET model = ?3, effort = ?4 WHERE job_id = ?1 AND session = ?2",
            params![job_id, session_key(session), model, effort],
        )?;
        Ok(())
    }

    /// The id the agent reported for `session` of job `job_id`, such as Codex's
    /// thread id, or `None` when the session has none yet or runs on an agent
    /// that resumes by the factory's UUID, as Claude does. A session that has
    /// never run has no row and reads `None`.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn agent_session(
        &self,
        job_id: i64,
        session: Session,
    ) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT agent_session FROM sessions WHERE job_id = ?1 AND session = ?2",
                params![job_id, session_key(session)],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Records `id` as the agent-reported session id of the session whose
    /// factory UUID is `uuid`, as soon as the run reports it, so a run that dies
    /// after that line resumes on the id. The UUID names one session row, so this
    /// touches no other. A UUID no row holds writes nothing.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn set_agent_session(&self, uuid: &str, id: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET agent_session = ?2 WHERE uuid = ?1",
            params![uuid, id],
        )?;
        Ok(())
    }

    /// The time of the newest pull request comment already given to the
    /// agent.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn pr_watermark(&self, job_id: i64, pr: usize) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT seen_until FROM pr_watermarks WHERE job_id = ?1 AND pr = ?2",
                params![job_id, i64::try_from(pr).unwrap_or(i64::MAX)],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Applies `event` to job `job_id` and records `seen_until` as the
    /// watermark of pull request `pr`, in one transaction.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::apply`], and then records nothing.
    pub fn apply_with_watermark(
        &self,
        job_id: i64,
        pr: usize,
        event: Event,
        seen_until: &str,
    ) -> Result<JobRow, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut row = Self::load(&tx, job_id)?;
        Self::apply_in(&tx, &mut row, event)?;
        tx.execute(
            "INSERT INTO pr_watermarks (job_id, pr, seen_until) VALUES (?1, ?2, ?3)
             ON CONFLICT (job_id, pr) DO UPDATE SET seen_until = excluded.seen_until",
            params![job_id, i64::try_from(pr).unwrap_or(i64::MAX), seen_until],
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// # Errors
    ///
    /// Fails on a database error.
    pub fn append_log(&self, job_id: i64, line: &LogLine) -> Result<(), StoreError> {
        let (kind, text) = match line {
            LogLine::Text(text) => ("text", text.clone()),
            LogLine::Tool { name, summary } if summary.is_empty() => ("tool", name.clone()),
            LogLine::Tool { name, summary } => ("tool", format!("{name}: {summary}")),
            LogLine::Other(text) => ("other", text.clone()),
        };
        self.append_log_entry(job_id, kind, &text)
    }

    /// A line from the factory itself, such as a run starting.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn append_factory_log(&self, job_id: i64, text: &str) -> Result<(), StoreError> {
        self.append_log_entry(job_id, "factory", text)
    }

    fn append_log_entry(&self, job_id: i64, kind: &str, text: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO logs (job_id, kind, text) VALUES (?1, ?2, ?3)",
            params![job_id, kind, text],
        )?;
        Ok(())
    }

    /// The last `limit` log lines of job `job_id`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn logs(&self, job_id: i64, limit: usize) -> Result<Vec<LogEntry>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT at, kind, text FROM
               (SELECT id, at, kind, text FROM logs WHERE job_id = ?1 ORDER BY id DESC LIMIT ?2)
             ORDER BY id",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![job_id, limit], |row| {
            Ok(LogEntry {
                at: row.get(0)?,
                kind: row.get(1)?,
                text: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Stores the opaque `value` for a secret under its `backend`, replacing
    /// any row already keyed by the same project, server and header. The
    /// encryption lives in the secret backend, so the store treats `value` as
    /// bytes it never interprets.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn put_secret(
        &self,
        at: SecretRef<'_>,
        backend: &str,
        value: &[u8],
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO secrets (project, server, header, backend, value)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (project, server, header)
             DO UPDATE SET backend = excluded.backend, value = excluded.value",
            params![at.project, at.server, at.header, backend, value],
        )?;
        Ok(())
    }

    /// The backend and the opaque stored bytes for a secret, or `None` when no
    /// row is keyed by this project, server and header.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn get_secret(&self, at: SecretRef<'_>) -> Result<Option<(String, Vec<u8>)>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT backend, value FROM secrets
                 WHERE project = ?1 AND server = ?2 AND header = ?3",
                params![at.project, at.server, at.header],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Deletes the secret for a project, server and header. A row that is not
    /// there is fine.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn delete_secret(&self, at: SecretRef<'_>) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM secrets WHERE project = ?1 AND server = ?2 AND header = ?3",
            params![at.project, at.server, at.header],
        )?;
        Ok(())
    }

    /// Every registered Forgejo instance, in name order.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn forgejo_instances(&self) -> Result<Vec<ForgejoInstance>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, base_url, version, added_at, registration FROM forgejo_instances ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ForgejoInstance {
                name: row.get(0)?,
                base_url: row.get(1)?,
                version: row.get(2)?,
                added_at: row.get(3)?,
                registration: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The forge named `name`: GitHub, Salsa or a registered Forgejo
    /// instance. `None` for any other name.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn resolve_forge(&self, name: &str) -> Result<Option<ForgeKind>, StoreError> {
        if let Some(kind) = ForgeKind::parse(name) {
            return Ok(Some(kind));
        }
        Ok(self.forgejo_instance(name)?.map(|instance| {
            ForgeKind::Forgejo(ForgejoServer {
                name: instance.name,
                base_url: instance.base_url,
                registration: instance.registration,
            })
        }))
    }

    /// Every forge a project can live on: GitHub, Salsa, then each
    /// registered Forgejo instance by name.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn forge_kinds(&self) -> Result<Vec<ForgeKind>, StoreError> {
        let mut kinds = ForgeKind::FIXED.to_vec();
        kinds.extend(self.forgejo_instances()?.into_iter().map(|instance| {
            ForgeKind::Forgejo(ForgejoServer {
                name: instance.name,
                base_url: instance.base_url,
                registration: instance.registration,
            })
        }));
        Ok(kinds)
    }

    /// The instance registered under `name`, or `None`.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn forgejo_instance(&self, name: &str) -> Result<Option<ForgejoInstance>, StoreError> {
        Ok(self
            .forgejo_instances()?
            .into_iter()
            .find(|instance| instance.name == name))
    }

    /// Checks that an instance with this name and base URL could be added:
    /// both are valid and neither is taken. Returns them in stored form.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Forgejo`] and a message when one is invalid or
    /// taken, or on a database error.
    pub fn check_new_forgejo_instance(
        &self,
        name: &str,
        url: &str,
    ) -> Result<(String, String), StoreError> {
        let (name, url) = ForgejoInstance::validate(name, url).map_err(StoreError::Forgejo)?;
        for existing in self.forgejo_instances()? {
            if existing.name == name {
                return Err(StoreError::Forgejo(format!(
                    "An instance named {name:?} already exists."
                )));
            }
            if existing.base_url == url {
                return Err(StoreError::Forgejo(format!(
                    "{url} is already registered as {:?}.",
                    existing.name
                )));
            }
        }
        Ok((name, url))
    }

    /// Registers an instance and stores the `version` it reported.
    ///
    /// # Errors
    ///
    /// As [`Store::check_new_forgejo_instance`].
    pub fn add_forgejo_instance(
        &self,
        name: &str,
        url: &str,
        version: &str,
    ) -> Result<ForgejoInstance, StoreError> {
        let (name, url) = self.check_new_forgejo_instance(name, url)?;
        let inserted = {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            tx.execute("INSERT INTO forgejo_registrations DEFAULT VALUES", [])?;
            let registration = tx.last_insert_rowid();
            tx.query_row(
                "INSERT INTO forgejo_instances (name, base_url, version, registration)
                 VALUES (?1, ?2, ?3, ?4) RETURNING added_at",
                params![name, url, version, registration],
                |row| row.get::<_, String>(0),
            )
            .and_then(|added_at| tx.commit().map(|()| (added_at, registration)))
        };
        // A concurrent add can pass the check above, so the table's unique
        // constraints decide and the check names the one that failed.
        let (added_at, registration) = match inserted {
            Ok(done) => done,
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                self.check_new_forgejo_instance(&name, &url)?;
                return Err(StoreError::Forgejo(format!(
                    "{name} or {url} is already registered."
                )));
            }
            Err(err) => return Err(err.into()),
        };
        Ok(ForgejoInstance {
            name,
            base_url: url,
            version: version.to_owned(),
            added_at,
            registration,
        })
    }

    /// Deletes an instance and every token stored for it: each login's token
    /// and account, and each project's token and account. A token is bound to
    /// the instance's name, so one left behind would be readable by an
    /// instance added later under the same name.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Forgejo`] when no such instance exists or a
    /// project uses it as its forge or its upstream forge. The message names
    /// the projects. Fails on a database error.
    pub fn delete_forgejo_instance(&self, name: &str) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let registered: i64 = tx.query_row(
            "SELECT COUNT(*) FROM forgejo_instances WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?;
        if registered == 0 {
            return Err(StoreError::Forgejo(format!("No instance named {name:?}.")));
        }
        let users: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT name FROM projects WHERE forge = ?1
                 UNION
                 SELECT project FROM project_maintenance WHERE upstream_forge = ?1
                 ORDER BY 1",
            )?;
            stmt.query_map([name], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        if !users.is_empty() {
            return Err(StoreError::Forgejo(format!(
                "Nothing was deleted. {name} is used by {}.",
                users.join(", ")
            )));
        }
        let prefix = format!("forge-user:{name}:");
        tx.execute(
            "DELETE FROM secrets
             WHERE project = '' AND server = '' AND substr(header, 1, length(?1)) = ?1",
            [&prefix],
        )?;
        tx.execute("DELETE FROM user_forge_accounts WHERE forge = ?1", [name])?;
        tx.execute(
            "DELETE FROM secrets WHERE server = '' AND (project, header) IN (
                 SELECT project, CASE kind WHEN 'upstream' THEN 'forge-upstream'
                                           ELSE 'forge-project' END
                 FROM project_forge_accounts WHERE forge = ?1)",
            [name],
        )?;
        tx.execute(
            "DELETE FROM project_forge_accounts WHERE forge = ?1",
            [name],
        )?;
        tx.execute("DELETE FROM forgejo_instances WHERE name = ?1", [name])?;
        tx.commit()?;
        Ok(())
    }

    /// Every registered MCP server, in name order.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored list that does not parse.
    pub fn mcp_servers(&self) -> Result<Vec<McpServer>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, kind, url, headers, command, args FROM mcp_servers ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, mcp_row(row, 1)?)))?;
        let mut servers = Vec::new();
        for row in rows {
            let (name, stored) = row?;
            servers.push(McpServer {
                name,
                transport: stored.into_transport()?,
            });
        }
        Ok(servers)
    }

    /// The server registered under `name`, or `None` when none is.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored list that does not parse.
    pub fn mcp_server(&self, name: &str) -> Result<Option<McpServer>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT kind, url, headers, command, args FROM mcp_servers WHERE name = ?1",
                [name],
                |row| mcp_row(row, 0),
            )
            .optional()?;
        let Some(stored) = row else {
            return Ok(None);
        };
        Ok(Some(McpServer {
            name: name.to_owned(),
            transport: stored.into_transport()?,
        }))
    }

    /// Registers a remote MCP server. See [`Store::create_mcp_server_with`].
    ///
    /// # Errors
    ///
    /// As [`Store::create_mcp_server_with`].
    pub fn create_mcp_server(
        &self,
        name: &str,
        url: &str,
        headers: &[String],
    ) -> Result<(), McpError> {
        self.create_mcp_server_with(
            name,
            &McpTransport::Remote {
                url: url.to_owned(),
                headers: headers.to_vec(),
            },
        )
    }

    /// Registers an MCP server. The name and the transport's fields are
    /// validated; header names are deduplicated case-insensitively and a stdio
    /// command is trimmed and its empty arguments dropped before storage.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::BadName`], [`McpError::BadUrl`],
    /// [`McpError::BadHeader`], [`McpError::ReservedHeader`],
    /// [`McpError::BadCommand`] or [`McpError::BadArgs`] for an invalid field,
    /// and [`McpError::DuplicateName`] when `name` is taken, by either kind.
    /// Nothing is written when it fails.
    pub fn create_mcp_server_with(
        &self,
        name: &str,
        transport: &McpTransport,
    ) -> Result<(), McpError> {
        mcp::validate_name(name)?;
        let stored = StoredTransport::validated(transport)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let taken = tx
            .query_row("SELECT 1 FROM mcp_servers WHERE name = ?1", [name], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if taken {
            return Err(McpError::DuplicateName(name.to_owned()));
        }
        tx.execute(
            "INSERT INTO mcp_servers (name, kind, url, headers, command, args)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                name,
                stored.kind,
                stored.url,
                stored.headers,
                stored.command,
                stored.args
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Edits a remote server. See [`Store::edit_mcp_server_with`].
    ///
    /// # Errors
    ///
    /// As [`Store::edit_mcp_server_with`].
    pub fn edit_mcp_server(
        &self,
        name: &str,
        url: &str,
        headers: &[String],
    ) -> Result<(), McpError> {
        self.edit_mcp_server_with(
            name,
            &McpTransport::Remote {
                url: url.to_owned(),
                headers: headers.to_vec(),
            },
        )
    }

    /// Edits the transport of the server `name`. The name itself does not
    /// change, because it keys the stored secrets and the proxy path, and the
    /// kind does not change either.
    ///
    /// For a remote server: when the new URL's origin (scheme, host or port)
    /// differs from the old one, every project's stored secret value for this
    /// server is deleted, so a redirected server cannot collect another
    /// project's keys, and every stored tool is marked as not offered until the
    /// next refresh. Each tool keeps its on or off state. Otherwise, each
    /// header name no longer declared has its stored values deleted. Both happen
    /// in the same transaction as the update.
    ///
    /// For a stdio server the command and base arguments are replaced and
    /// every project's settings for it stay, because it has no stored values.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::BadUrl`], [`McpError::BadHeader`],
    /// [`McpError::ReservedHeader`], [`McpError::BadCommand`] or
    /// [`McpError::BadArgs`] for an invalid field, [`McpError::KindChange`] when
    /// `transport` is not the server's kind, and [`McpError::NotFound`] when no
    /// server is registered under `name`. Nothing is written when it fails.
    pub fn edit_mcp_server_with(
        &self,
        name: &str,
        transport: &McpTransport,
    ) -> Result<(), McpError> {
        let stored = StoredTransport::validated(transport)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let existing = tx
            .query_row(
                "SELECT kind, url, headers, command, args FROM mcp_servers WHERE name = ?1",
                [name],
                |row| mcp_row(row, 0),
            )
            .optional()?;
        let Some(existing) = existing else {
            return Err(McpError::NotFound(name.to_owned()));
        };
        if existing.kind != stored.kind {
            return Err(McpError::KindChange(name.to_owned()));
        }
        if let McpTransport::Remote { url, .. } = transport {
            let headers: Vec<String> = serde_json::from_str(&stored.headers)?;
            if mcp::same_origin(&existing.url, url) {
                let old_headers: Vec<String> = serde_json::from_str(&existing.headers)?;
                for removed in old_headers
                    .iter()
                    .filter(|old| !headers.iter().any(|kept| kept.eq_ignore_ascii_case(old)))
                {
                    tx.execute(
                        "DELETE FROM secrets WHERE server = ?1 AND header = ?2",
                        params![name, removed],
                    )?;
                }
            } else {
                tx.execute("DELETE FROM secrets WHERE server = ?1", [name])?;
                tx.execute(
                    "UPDATE project_mcp_tools SET offered = 0 WHERE server = ?1",
                    [name],
                )?;
            }
        }
        tx.execute(
            "UPDATE mcp_servers SET url = ?1, headers = ?2, command = ?3, args = ?4
             WHERE name = ?5",
            params![
                stored.url,
                stored.headers,
                stored.command,
                stored.args,
                name
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The stored header values for server `name`, as `(project, header)`
    /// pairs in project and header order. The values themselves stay sealed.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn mcp_secrets(&self, name: &str) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT project, header FROM secrets WHERE server = ?1 ORDER BY project, header",
        )?;
        let rows = stmt.query_map([name], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The stored values that [`Store::edit_mcp_server`] would delete for the
    /// same arguments, as `(project, header)` pairs, without writing anything:
    /// every value when the URL's origin changes, else the values of each
    /// removed header. The dashboard asks before a save that deletes any.
    ///
    /// # Errors
    ///
    /// Fails as [`Store::edit_mcp_server`] would, on an invalid URL or header
    /// name, an unknown server or a database error. A stdio server gives an
    /// empty list.
    pub fn mcp_edit_deletes(
        &self,
        name: &str,
        url: &str,
        headers: &[String],
    ) -> Result<Vec<(String, String)>, McpError> {
        mcp::validate_url(url)?;
        let headers = mcp::normalize_headers(headers)?;
        let Some(server) = self.mcp_server(name)? else {
            return Err(McpError::NotFound(name.to_owned()));
        };
        // A stdio server has no stored values, so an edit deletes none.
        let Some((current, _)) = server.as_remote() else {
            return Ok(Vec::new());
        };
        let stored = self.mcp_secrets(name)?;
        if !mcp::same_origin(current, url) {
            return Ok(stored);
        }
        Ok(stored
            .into_iter()
            .filter(|(_, header)| !headers.iter().any(|kept| kept.eq_ignore_ascii_case(header)))
            .collect())
    }

    /// Deletes the server `name` with every project's settings, stored secret
    /// values and stored tools for it, in one transaction. Deleting a server that is not registered is fine.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn delete_mcp_server(&self, name: &str) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM secrets WHERE server = ?1", [name])?;
        tx.execute("DELETE FROM project_mcp WHERE server = ?1", [name])?;
        tx.execute("DELETE FROM project_mcp_tools WHERE server = ?1", [name])?;
        tx.execute("DELETE FROM project_mcp_args WHERE server = ?1", [name])?;
        tx.execute(
            "DELETE FROM project_mcp_tool_refreshes WHERE server = ?1",
            [name],
        )?;
        tx.execute("DELETE FROM mcp_servers WHERE name = ?1", [name])?;
        tx.commit()?;
        Ok(())
    }

    /// The settings `project` has for the MCP server `server`, or the default
    /// (off, no stage) when no row is stored. A project with no row, a newly
    /// onboarded project and a newly added server all read the default.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn project_mcp(
        &self,
        project: &str,
        server: &str,
    ) -> Result<McpProjectSettings, StoreError> {
        let stored = self
            .conn()
            .query_row(
                "SELECT enabled, clarify, spec, plan, implement, review
                 FROM project_mcp WHERE project = ?1 AND server = ?2",
                params![project, server],
                |row| {
                    Ok(McpProjectSettings {
                        enabled: row.get(0)?,
                        stages: McpStages {
                            clarify: row.get(1)?,
                            spec: row.get(2)?,
                            plan: row.get(3)?,
                            implement: row.get(4)?,
                            review: row.get(5)?,
                        },
                    })
                },
            )
            .optional()?;
        Ok(stored.unwrap_or_default())
    }

    /// Saves `project`'s settings for `server`, writing only this row. An
    /// enabled server must have at least one stage; otherwise nothing is written
    /// and [`McpError::NoStage`] is returned.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::NoStage`] when `settings.enabled` is set with no
    /// stage. Otherwise fails only on a database error.
    pub fn save_project_mcp(
        &self,
        project: &str,
        server: &str,
        settings: &McpProjectSettings,
    ) -> Result<(), McpError> {
        if settings.enabled && !settings.stages.any() {
            return Err(McpError::NoStage(server.to_owned()));
        }
        let stages = settings.stages;
        self.conn().execute(
            "INSERT INTO project_mcp
                (project, server, enabled, clarify, spec, plan, implement, review)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (project, server) DO UPDATE SET enabled = excluded.enabled,
                clarify = excluded.clarify, spec = excluded.spec, plan = excluded.plan,
                implement = excluded.implement, review = excluded.review",
            params![
                project,
                server,
                settings.enabled,
                stages.clarify,
                stages.spec,
                stages.plan,
                stages.implement,
                stages.review
            ],
        )?;
        Ok(())
    }

    /// The extra arguments `project` appends after the base arguments of the
    /// stdio server `server`, in order. Empty when no row is stored.
    ///
    /// # Errors
    ///
    /// Fails on a database error or when the stored list is not valid JSON.
    pub fn project_mcp_args(&self, project: &str, server: &str) -> Result<Vec<String>, StoreError> {
        let stored: Option<String> = self
            .conn()
            .query_row(
                "SELECT args FROM project_mcp_args WHERE project = ?1 AND server = ?2",
                params![project, server],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(Vec::new()),
        }
    }

    /// Saves `project`'s extra arguments for `server`, writing only this row.
    /// The caller has validated them with [`crate::mcp::validate_args`].
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn save_project_mcp_args(
        &self,
        project: &str,
        server: &str,
        args: &[String],
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(args)?;
        self.conn().execute(
            "INSERT INTO project_mcp_args (project, server, args) VALUES (?1, ?2, ?3)
             ON CONFLICT (project, server) DO UPDATE SET args = excluded.args",
            params![project, server, json],
        )?;
        Ok(())
    }

    /// The tools stored for `project`'s use of `server`, sorted by name. Empty
    /// until a refresh has listed the server's tools.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn mcp_tools(&self, project: &str, server: &str) -> Result<Vec<McpToolState>, StoreError> {
        let conn = self.conn();
        let latest: i64 = conn
            .query_row(
                "SELECT latest FROM project_mcp_tool_refreshes WHERE project = ?1 AND server = ?2",
                params![project, server],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut stmt = conn.prepare(
            "SELECT tool, description, offered, disabled, first_seen FROM project_mcp_tools
             WHERE project = ?1 AND server = ?2 ORDER BY tool",
        )?;
        let rows = stmt.query_map(params![project, server], |row| {
            let first_seen: i64 = row.get(4)?;
            Ok(McpToolState {
                name: row.get(0)?,
                description: row.get(1)?,
                offered: row.get(2)?,
                disabled: row.get(3)?,
                new: latest > 1 && first_seen == latest,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The names of the tools `project` turned off for `server`. The proxy
    /// refuses a call to any of them.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn disabled_mcp_tools(
        &self,
        project: &str,
        server: &str,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT tool FROM project_mcp_tools
             WHERE project = ?1 AND server = ?2 AND disabled = 1",
        )?;
        let rows = stmt.query_map(params![project, server], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replaces the stored tool list of `project`'s use of `server` with
    /// `tools`, the complete reply of one refresh. A listed tool is marked
    /// offered and its description updated. A stored tool missing from `tools`
    /// keeps its row, marked not offered, so a tool the project turned off stays
    /// off if the server offers it again. Each tool keeps its on or off state,
    /// and a tool seen for the first time starts on.
    ///
    /// # Errors
    ///
    /// Fails on a database error. Nothing is written then.
    pub fn replace_mcp_tools(
        &self,
        project: &str,
        server: &str,
        tools: &[McpTool],
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO project_mcp_tool_refreshes (project, server, latest) VALUES (?1, ?2, 1)
             ON CONFLICT (project, server) DO UPDATE SET latest = latest + 1",
            params![project, server],
        )?;
        let latest: i64 = tx.query_row(
            "SELECT latest FROM project_mcp_tool_refreshes WHERE project = ?1 AND server = ?2",
            params![project, server],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE project_mcp_tools SET offered = 0 WHERE project = ?1 AND server = ?2",
            params![project, server],
        )?;
        for tool in tools {
            tx.execute(
                "INSERT INTO project_mcp_tools
                    (project, server, tool, description, offered, disabled, first_seen)
                 VALUES (?1, ?2, ?3, ?4, 1, 0, ?5)
                 ON CONFLICT (project, server, tool) DO UPDATE SET
                    description = excluded.description, offered = 1",
                params![project, server, tool.name, tool.description, latest],
            )?;
        }
        Ok(tx.commit()?)
    }

    /// Turns each named tool of `project`'s use of `server` on or off, `true`
    /// meaning off. A name with no stored row is skipped, so a form that shows
    /// a tool a later edit cleared writes nothing for it.
    ///
    /// # Errors
    ///
    /// Fails on a database error. Nothing is written then.
    pub fn save_mcp_tool_states(
        &self,
        project: &str,
        server: &str,
        states: &[(String, bool)],
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (tool, disabled) in states {
            tx.execute(
                "UPDATE project_mcp_tools SET disabled = ?4
                 WHERE project = ?1 AND server = ?2 AND tool = ?3",
                params![project, server, tool, disabled],
            )?;
        }
        Ok(tx.commit()?)
    }

    /// Whether a secret value is stored for a project, server and header,
    /// without decrypting it. The settings page shows "set" or "not set" from
    /// this, so the state reads the same when the master key is missing.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn has_secret(&self, at: SecretRef<'_>) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM secrets WHERE project = ?1 AND server = ?2 AND header = ?3",
                params![at.project, at.server, at.header],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn load(conn: &Connection, id: i64) -> Result<JobRow, StoreError> {
        let raw = conn
            .query_row(
                "SELECT id, project, requested_by, created_at, job FROM jobs WHERE id = ?1",
                [id],
                raw_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound(id))?;
        parse_row(raw)
    }
}

/// A prerequisite of a job as the job page shows it.
#[derive(Debug, Clone)]
pub struct PrerequisiteView {
    /// The `job_prerequisites` row, which identifies the prerequisite where
    /// `id` may name a different job after the original was deleted.
    pub row: i64,
    pub id: i64,
    /// The first line of the request, as stored when the job was deleted.
    pub title: String,
    pub state: PrerequisiteState,
    /// The prerequisite job, or `None` when it was deleted. A deleted one is
    /// met when it had merged and blocked otherwise.
    pub job: Option<Job>,
}

/// The prerequisite set an edit posts. `keep` names the `job_prerequisites`
/// rows of the job to keep, live or deleted. A row id identifies a prerequisite
/// where a job id cannot, because a deleted job's id may belong to a new job.
/// `after` names jobs to add.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrerequisiteEdit<'a> {
    pub keep: &'a [i64],
    pub after: &'a [i64],
}

struct PrerequisiteChange {
    keep: Vec<i64>,
    added: Vec<i64>,
    names: Vec<i64>,
    changed: bool,
}

type PrerequisiteEntry = (i64, PrerequisiteState, String);

/// Swaps a queued job's staged attachment directories into place while keeping
/// the old set aside, so a failure before the store commits can restore it
/// exactly. [`DirSwap::swap_in`] moves `images/` and `files/` to `.bak` names
/// and the `staging/` set over them, recording each rename. `commit` drops the
/// `.bak` set once the transaction has committed; `undo` reverses every rename.
struct DirSwap {
    /// Each rename performed, as `(from, to)`, in the order done.
    done: Vec<(PathBuf, PathBuf)>,
    /// The `.bak` directories holding the old set, removed on `commit`.
    backups: Vec<PathBuf>,
}

impl DirSwap {
    /// Moves the current `images/` and `files/` aside to `.bak` names, then
    /// moves the `staging/images` and `staging/files` set into their place. A
    /// directory that does not exist is skipped, so a job with no images or no
    /// files swaps only what it has. On any failure the partial swap is undone
    /// and the error returned, leaving the old set live.
    fn swap_in(job_dir: &Path) -> Result<DirSwap, StoreError> {
        let mut swap = DirSwap {
            done: Vec::new(),
            backups: Vec::new(),
        };
        match swap.try_swap(job_dir) {
            Ok(()) => Ok(swap),
            Err(err) => {
                swap.undo();
                Err(err)
            }
        }
    }

    fn try_swap(&mut self, job_dir: &Path) -> Result<(), StoreError> {
        let images = job_dir.join("images");
        let files = job_dir.join("files");
        let images_bak = job_dir.join("images.bak");
        let files_bak = job_dir.join("files.bak");
        let staging = job_dir.join("staging");
        if self.rename_opt(&images, &images_bak)? {
            self.backups.push(images_bak.clone());
        }
        if self.rename_opt(&files, &files_bak)? {
            self.backups.push(files_bak.clone());
        }
        self.rename_opt(&staging.join("images"), &images)?;
        self.rename_opt(&staging.join("files"), &files)?;
        Ok(())
    }

    /// Renames `from` to `to` when `from` exists, recording the move. Returns
    /// whether a rename happened.
    fn rename_opt(&mut self, from: &Path, to: &Path) -> Result<bool, StoreError> {
        if from.exists() {
            std::fs::rename(from, to).map_err(StoreError::Io)?;
            self.done.push((from.to_owned(), to.to_owned()));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Reverses every recorded rename, newest first, restoring the old set.
    fn undo(self) {
        for (from, to) in self.done.into_iter().rev() {
            let _ = std::fs::rename(&to, &from);
        }
    }

    /// Drops the saved old set once the swap is permanent.
    fn commit(self) {
        for bak in &self.backups {
            let _ = std::fs::remove_dir_all(bak);
        }
    }
}

/// A `projects` row whose first column is `name`, as `list_projects` selects.
/// The `session` column value for `session`, the key each session row carries
/// next to its job id.
fn session_key(session: Session) -> String {
    match session {
        Session::Planning => "planning".to_owned(),
        Session::Spec => "spec".to_owned(),
        Session::Plan => "plan".to_owned(),
        Session::Pr(pr) => format!("pr-{pr}"),
        Session::Review(pr) => format!("review-{pr}"),
        Session::Renovate(run) => format!("renovate-{run}"),
    }
}

/// The base URL and registration number in columns `at` and `at + 1`, `None`
/// for a name with no registered instance.
fn registered(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Option<(String, i64)>> {
    let url: Option<String> = row.get(at)?;
    let registration: Option<i64> = row.get(at + 1)?;
    Ok(url.zip(registration))
}

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        forge: ProjectForge::resolve(
            &row.get::<_, String>(1)?,
            registered(row, 5)?
                .as_ref()
                .map(|(url, id)| (url.as_str(), *id)),
        ),
        repo: row.get(2)?,
        base: row.get(3)?,
        checks: row.get(4)?,
        models: StageModels::default(),
        auto_merge: false,
        llm_review: false,
    })
}

/// A `projects` row with no `name` column, as `project` selects.
fn project_row_without_name(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        forge: ProjectForge::resolve(
            &row.get::<_, String>(0)?,
            registered(row, 4)?
                .as_ref()
                .map(|(url, id)| (url.as_str(), *id)),
        ),
        repo: row.get(1)?,
        base: row.get(2)?,
        checks: row.get(3)?,
        models: StageModels::default(),
        auto_merge: false,
        llm_review: false,
    })
}

type RawRow = (i64, String, String, String, String);

fn raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn parse_row((id, project, requested_by, created_at, job): RawRow) -> Result<JobRow, StoreError> {
    Ok(JobRow {
        id,
        project,
        requested_by,
        created_at,
        job: serde_json::from_str(&job)?,
    })
}

fn project_token_kind(owner: ForgeTokenOwner<'_>) -> &'static str {
    match owner {
        ForgeTokenOwner::Upstream { .. } => "upstream",
        _ => "project",
    }
}

/// Fails with [`StoreError::InstanceChanged`] unless `forge` is a fixed forge
/// or a Forgejo instance whose current registration is this one.
fn require_registered(conn: &Connection, forge: &ForgeKind) -> Result<(), StoreError> {
    let ForgeKind::Forgejo(server) = forge else {
        return Ok(());
    };
    let registered: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM forgejo_instances WHERE name = ?1 AND registration = ?2)",
        params![server.name, server.registration],
        |row| row.get(0),
    )?;
    if registered {
        Ok(())
    } else {
        Err(StoreError::InstanceChanged(server.name.clone()))
    }
}

/// Saving or clearing a project token ends the startup copy of `GH_TOKEN` for
/// that project, in the transaction that changes the token.
fn mark_project_token_handled(
    conn: &Connection,
    owner: ForgeTokenOwner<'_>,
) -> Result<(), StoreError> {
    if let ForgeTokenOwner::Project { project, .. } = owner {
        conn.execute(
            "INSERT OR IGNORE INTO gh_token_copies (project) VALUES (?1)",
            [project],
        )?;
    }
    Ok(())
}

fn write_forge_account(
    conn: &Connection,
    owner: ForgeTokenOwner<'_>,
    account: &ForgeAccount,
) -> Result<(), StoreError> {
    match owner {
        ForgeTokenOwner::User { login, forge } => conn.execute(
            "INSERT INTO user_forge_accounts (login, forge, account_login, account_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (login, forge) DO UPDATE SET
                 account_login = excluded.account_login,
                 account_id = excluded.account_id",
            params![
                login,
                forge.as_str(),
                account.login,
                account.id.cast_signed()
            ],
        )?,
        ForgeTokenOwner::Project { project, forge }
        | ForgeTokenOwner::Upstream { project, forge } => conn.execute(
            "INSERT INTO project_forge_accounts
                 (project, kind, forge, account_login, account_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (project, kind) DO UPDATE SET
                 forge = excluded.forge,
                 account_login = excluded.account_login,
                 account_id = excluded.account_id",
            params![
                project,
                project_token_kind(owner),
                forge.as_str(),
                account.login,
                account.id.cast_signed()
            ],
        )?,
    };
    Ok(())
}

fn erase_forge_account(conn: &Connection, owner: ForgeTokenOwner<'_>) -> Result<(), StoreError> {
    match owner {
        ForgeTokenOwner::User { login, forge } => conn.execute(
            "DELETE FROM user_forge_accounts WHERE login = ?1 AND forge = ?2",
            params![login, forge.as_str()],
        )?,
        ForgeTokenOwner::Project { project, .. } | ForgeTokenOwner::Upstream { project, .. } => {
            conn.execute(
                "DELETE FROM project_forge_accounts WHERE project = ?1 AND kind = ?2",
                params![project, project_token_kind(owner)],
            )?
        }
    };
    Ok(())
}

/// What [`Store::save_project_maintenance_guarded`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceSave {
    Saved,
    /// The project or the collaborator no longer exists.
    Gone,
    /// A token that was checked is no longer the stored one.
    TokenChanged,
    /// A Forgejo instance the save uses was deleted, or registered again at
    /// another address, while the save was checked.
    InstanceChanged,
}

fn write_maintenance(
    conn: &Connection,
    project: &str,
    maintenance: &Maintenance,
) -> Result<(), StoreError> {
    let packaging = maintenance.packaging.as_ref();
    conn.execute(
        "INSERT INTO project_maintenance
            (project, renovate, packaging, packaging_type, manifest_files,
             build_config_files, upstream_repo, renovate_author, upstream_forge)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (project) DO UPDATE SET renovate = excluded.renovate,
            packaging = excluded.packaging,
            packaging_type = excluded.packaging_type,
            manifest_files = excluded.manifest_files,
            build_config_files = excluded.build_config_files,
            upstream_repo = excluded.upstream_repo,
            renovate_author = excluded.renovate_author,
            upstream_forge = excluded.upstream_forge",
        params![
            project,
            maintenance.renovate,
            packaging.is_some(),
            packaging.map(|p| p.kind.as_str()),
            serde_json::to_string(&packaging.map_or(&[][..], |p| &p.manifest_files[..]))?,
            serde_json::to_string(&packaging.map_or(&[][..], |p| &p.build_config_files[..]))?,
            packaging.map(|p| p.upstream_repo.as_str()),
            maintenance.renovate_author,
            packaging.map(|p| p.upstream_forge.as_str()),
        ],
    )?;
    Ok(())
}

fn read_forge_account(
    conn: &Connection,
    owner: ForgeTokenOwner<'_>,
) -> Result<Option<ForgeAccount>, StoreError> {
    let found = match owner {
        ForgeTokenOwner::User { login, forge } => conn.query_row(
            "SELECT account_login, account_id FROM user_forge_accounts
             WHERE login = ?1 AND forge = ?2",
            params![login, forge.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        ),
        ForgeTokenOwner::Project { project, forge }
        | ForgeTokenOwner::Upstream { project, forge } => conn.query_row(
            "SELECT account_login, account_id FROM project_forge_accounts
                 WHERE project = ?1 AND kind = ?2 AND forge = ?3",
            params![project, project_token_kind(owner), forge.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        ),
    }
    .optional()?;
    Ok(found.map(|(login, id)| ForgeAccount {
        login,
        id: id.cast_unsigned(),
    }))
}
