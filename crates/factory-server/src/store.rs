use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use factory_core::{Event, Job, Rejected, Session, Stage};
use factory_runner::LogLine;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::projects::StageModels;
use crate::settings::Settings;

const SCHEMA: &str = "
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
CREATE TABLE IF NOT EXISTS pr_watermarks (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    pr INTEGER NOT NULL,
    seen_until TEXT NOT NULL,
    PRIMARY KEY (job_id, pr)
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
    Db(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(id) => write!(f, "no job {id}"),
            StoreError::Rejected(rejected) => rejected.fmt(f),
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
        let job = serde_json::to_string(&Job::new(request, gates))?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO jobs (project, requested_by, job) VALUES (?1, ?2, ?3)",
            params![project, requested_by, job],
        )?;
        let id = tx.last_insert_rowid();
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

    /// Applies `event` to `row`, saves the job and counts a first arrival at
    /// `Merged` in the project totals.
    fn apply_in(tx: &Transaction<'_>, row: &mut JobRow, event: Event) -> Result<(), StoreError> {
        let was_merged = *row.job.stage() == Stage::Merged;
        row.job.apply(event).map_err(StoreError::Rejected)?;
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
