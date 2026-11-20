use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use factory_core::{AgentResult, EFFORTS, Event, Job, Rejected, Session, Stage};
use factory_runner::LogLine;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::mcp::{self, McpError, McpProjectSettings, McpServer, McpStages};
use crate::projects::{Project, StageEfforts, StageModels};
use crate::secrets::SecretRef;
use crate::settings::Settings;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    name TEXT PRIMARY KEY,
    repo TEXT NOT NULL UNIQUE,
    base TEXT NOT NULL,
    checks TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    job TEXT NOT NULL
);
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
CREATE TABLE IF NOT EXISTS project_efforts (
    project TEXT PRIMARY KEY,
    clarify TEXT NOT NULL,
    spec TEXT NOT NULL,
    plan TEXT NOT NULL,
    implement TEXT NOT NULL,
    review TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    session TEXT NOT NULL,
    uuid TEXT NOT NULL,
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
    usd REAL
);
CREATE INDEX IF NOT EXISTS job_events_by_job ON job_events (job_id, id);
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
    headers TEXT NOT NULL
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
";

/// One line of a job's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// UTC, as SQLite's `CURRENT_TIMESTAMP` writes it.
    pub at: String,
    /// `text`, `tool`, `other` for agent output, or `factory`.
    pub kind: String,
    pub text: String,
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

/// The jobs, kept in SQLite. Each job is stored as JSON in one row.
pub struct Store {
    conn: Mutex<Connection>,
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
    /// A project with this `owner/name` repo is already onboarded.
    DuplicateRepo(String),
    /// An effort level outside the allowed set was submitted. It is not stored.
    InvalidEffort(String),
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
            StoreError::DuplicateRepo(repo) => {
                write!(f, "the repository {repo:?} is already onboarded")
            }
            StoreError::InvalidEffort(effort) => {
                write!(f, "{effort:?} is not an allowed effort level")
            }
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

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA)?;
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
        Ok(Store {
            conn: Mutex::new(conn),
        })
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
        let mut stmt =
            conn.prepare("SELECT name, repo, base, checks FROM projects ORDER BY name")?;
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
                "SELECT repo, base, checks FROM projects WHERE name = ?1",
                [name],
                project_row_without_name,
            )
            .optional()?)
    }

    /// Onboards a project with its `repo`, `base` and optional `checks`. Its
    /// models and flags start at their defaults; the settings page saves them
    /// into `project_models` and `project_settings`.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::DuplicateName`] when `name` is taken, with
    /// [`StoreError::DuplicateRepo`] when `repo` is already onboarded, and on
    /// a database error. Nothing is written when it fails.
    pub fn create_project(
        &self,
        name: &str,
        repo: &str,
        base: &str,
        checks: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let name_taken = tx
            .query_row("SELECT 1 FROM projects WHERE name = ?1", [name], |_| Ok(()))
            .optional()?
            .is_some();
        if name_taken {
            return Err(StoreError::DuplicateName(name.to_owned()));
        }
        let repo_taken = tx
            .query_row("SELECT 1 FROM projects WHERE repo = ?1", [repo], |_| Ok(()))
            .optional()?
            .is_some();
        if repo_taken {
            return Err(StoreError::DuplicateRepo(repo.to_owned()));
        }
        tx.execute(
            "INSERT INTO projects (name, repo, base, checks) VALUES (?1, ?2, ?3, ?4)",
            params![name, repo, base, checks],
        )?;
        tx.commit()?;
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
        let gates = self.settings()?.gates;
        let new = Job::new(request, gates);
        let job = serde_json::to_string(&new)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO jobs (project, requested_by, job) VALUES (?1, ?2, ?3)",
            params![project, requested_by, job],
        )?;
        let id = tx.last_insert_rowid();
        Self::record(&tx, id, "created", &new, None, None)?;
        tx.execute(
            "INSERT INTO project_totals (project, jobs_created) VALUES (?1, 1)
             ON CONFLICT (project) DO UPDATE SET jobs_created = jobs_created + 1",
            [project],
        )?;
        tx.commit()?;
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
        Self::load(&self.conn(), id)?;
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
            "job_costs",
            "job_events",
            "pr_watermarks",
            "logs",
            "sessions",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE job_id = ?1"), [id])?;
        }
        tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        Ok(tx.commit()?)
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

    /// Applies `event` to `row`, saves the job, adds the event to its history
    /// and counts a first arrival at `Merged` in the project totals.
    fn apply_in(tx: &Transaction<'_>, row: &mut JobRow, event: Event) -> Result<(), StoreError> {
        let was_merged = *row.job.stage() == Stage::Merged;
        let name = event.name();
        let detail = match &event {
            Event::AgentFailed { reason }
            | Event::ReviewFailed { reason }
            | Event::Agent(AgentResult::RerunCi { reason }) => Some(reason.clone()),
            _ => None,
        };
        row.job.apply(event).map_err(StoreError::Rejected)?;
        Self::record(tx, row.id, name, &row.job, detail.as_deref(), None)?;
        tx.execute(
            "UPDATE jobs SET job = ?1 WHERE id = ?2",
            params![serde_json::to_string(&row.job)?, row.id],
        )?;
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

    /// Adds `usd` to the running API cost of job `id` and to the totals of
    /// its project.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::NotFound`] when there is no job `id`, and on
    /// a database error.
    pub fn add_cost(&self, id: i64, usd: f64) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row = Self::load(&tx, id)?;
        Self::record(&tx, id, "cost", &row.job, None, Some(usd))?;
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
        Ok(tx.commit()?)
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

    fn record(
        tx: &Transaction<'_>,
        id: i64,
        event: &str,
        job: &Job,
        detail: Option<&str>,
        usd: Option<f64>,
    ) -> Result<(), StoreError> {
        let stage = job.stage();
        tx.execute(
            "INSERT INTO job_events (job_id, event, stage, pr, turn, detail, usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                event,
                stage.name(),
                stage.pr().map(|pr| i64::try_from(pr).unwrap_or(i64::MAX)),
                job.turn().name(),
                detail,
                usd
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
            "SELECT at, event, stage, pr, turn, detail, usd FROM job_events
             WHERE job_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |row| {
            let pr: Option<i64> = row.get(3)?;
            Ok(JobEvent {
                at: row.get(0)?,
                event: row.get(1)?,
                stage: row.get(2)?,
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
        let usd = self
            .conn()
            .query_row("SELECT usd FROM job_costs WHERE job_id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(usd.unwrap_or(0.0))
    }

    /// The API cost in USD of every job that has a `job_costs` row, by job id.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn costs(&self) -> Result<HashMap<i64, f64>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT job_id, usd FROM job_costs")?;
        let costs = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(costs)
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

    /// The stored settings, or the defaults when none are stored.
    ///
    /// # Errors
    ///
    /// Fails on a database error or stored settings that do not parse.
    pub fn settings(&self) -> Result<Settings, StoreError> {
        let stored: Option<String> = self
            .conn()
            .query_row("SELECT settings FROM settings WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(match stored {
            Some(json) => serde_json::from_str(&json)?,
            None => Settings::default(),
        })
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

    /// The effort levels saved for `project`, or the default of
    /// [`DEFAULT_EFFORT`](factory_core::DEFAULT_EFFORT) for every stage when no
    /// row is saved. A project in a database made before this table existed has
    /// no row and so reads every stage at the default.
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

    /// Saves `project`'s effort for all five stages, writing only this
    /// project's row. Every level must be one the `claude` CLI accepts, see
    /// [`EFFORTS`]; a value outside that set is rejected and nothing is
    /// written.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidEffort`] for a level outside the allowed
    /// set, before any write. Otherwise fails only on a database error.
    pub fn save_project_efforts(
        &self,
        project: &str,
        efforts: &StageEfforts,
    ) -> Result<(), StoreError> {
        for level in [
            &efforts.clarify,
            &efforts.spec,
            &efforts.plan,
            &efforts.implement,
            &efforts.review,
        ] {
            if !EFFORTS.contains(&level.as_str()) {
                return Err(StoreError::InvalidEffort(level.clone()));
            }
        }
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

    /// The Claude Code session UUID for `session` of job `job_id`, created
    /// on first use.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn session_uuid(&self, job_id: i64, session: Session) -> Result<String, StoreError> {
        let key = match session {
            Session::Planning => "planning".to_owned(),
            Session::Spec => "spec".to_owned(),
            Session::Plan => "plan".to_owned(),
            Session::Pr(pr) => format!("pr-{pr}"),
            Session::Review(pr) => format!("review-{pr}"),
        };
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (job_id, session, uuid) VALUES (?1, ?2, ?3)",
            params![job_id, key, uuid::Uuid::new_v4().to_string()],
        )?;
        Ok(conn.query_row(
            "SELECT uuid FROM sessions WHERE job_id = ?1 AND session = ?2",
            params![job_id, key],
            |row| row.get(0),
        )?)
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

    /// Every registered MCP server, in name order.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored header list that does not parse.
    pub fn mcp_servers(&self) -> Result<Vec<McpServer>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name, url, headers FROM mcp_servers ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut servers = Vec::new();
        for row in rows {
            let (name, url, headers) = row?;
            servers.push(McpServer {
                name,
                url,
                headers: serde_json::from_str(&headers)?,
            });
        }
        Ok(servers)
    }

    /// The server registered under `name`, or `None` when none is.
    ///
    /// # Errors
    ///
    /// Fails on a database error or a stored header list that does not parse.
    pub fn mcp_server(&self, name: &str) -> Result<Option<McpServer>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT url, headers FROM mcp_servers WHERE name = ?1",
                [name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((url, headers)) = row else {
            return Ok(None);
        };
        Ok(Some(McpServer {
            name: name.to_owned(),
            url,
            headers: serde_json::from_str(&headers)?,
        }))
    }

    /// Registers an MCP server. The name, URL and header names are validated,
    /// and header names are deduplicated case-insensitively before storage.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::BadName`], [`McpError::BadUrl`],
    /// [`McpError::BadHeader`] or [`McpError::ReservedHeader`] for an invalid
    /// field, and [`McpError::DuplicateName`] when `name` is taken. Nothing is
    /// written when it fails.
    pub fn create_mcp_server(
        &self,
        name: &str,
        url: &str,
        headers: &[String],
    ) -> Result<(), McpError> {
        mcp::validate_name(name)?;
        mcp::validate_url(url)?;
        let headers = mcp::normalize_headers(headers)?;
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
            "INSERT INTO mcp_servers (name, url, headers) VALUES (?1, ?2, ?3)",
            params![name, url, serde_json::to_string(&headers)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Edits the URL and header names of the server `name`. The name itself does
    /// not change, because it keys the stored secrets and the proxy path.
    ///
    /// When the new URL's origin (scheme, host or port) differs from the old
    /// one, every project's stored secret value for this server is deleted, so a
    /// redirected server cannot collect another project's keys. Otherwise, each
    /// header name no longer declared has its stored values deleted. Both happen
    /// in the same transaction as the update.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::BadUrl`], [`McpError::BadHeader`] or
    /// [`McpError::ReservedHeader`] for an invalid field, and
    /// [`McpError::NotFound`] when no server is registered under `name`. Nothing
    /// is written when it fails.
    pub fn edit_mcp_server(
        &self,
        name: &str,
        url: &str,
        headers: &[String],
    ) -> Result<(), McpError> {
        mcp::validate_url(url)?;
        let headers = mcp::normalize_headers(headers)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let existing = tx
            .query_row(
                "SELECT url, headers FROM mcp_servers WHERE name = ?1",
                [name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((old_url, old_headers)) = existing else {
            return Err(McpError::NotFound(name.to_owned()));
        };
        if mcp::same_origin(&old_url, url) {
            let old_headers: Vec<String> = serde_json::from_str(&old_headers)?;
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
        }
        tx.execute(
            "UPDATE mcp_servers SET url = ?1, headers = ?2 WHERE name = ?3",
            params![url, serde_json::to_string(&headers)?, name],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes the server `name` with every project's stored secret value for
    /// it, in one transaction. Deleting a server that is not registered is fine.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn delete_mcp_server(&self, name: &str) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM secrets WHERE server = ?1", [name])?;
        tx.execute("DELETE FROM project_mcp WHERE server = ?1", [name])?;
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

/// A `projects` row whose first column is `name`, as `list_projects` selects.
fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        repo: row.get(1)?,
        base: row.get(2)?,
        checks: row.get(3)?,
        models: StageModels::default(),
        auto_merge: false,
        llm_review: false,
    })
}

/// A `projects` row with no `name` column, as `project` selects.
fn project_row_without_name(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        repo: row.get(0)?,
        base: row.get(1)?,
        checks: row.get(2)?,
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
