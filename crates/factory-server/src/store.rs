use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use factory_core::{Event, Job, Rejected, Session};
use factory_runner::LogLine;
use rusqlite::{Connection, OptionalExtension, params};

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
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(id) => write!(f, "no job {id}"),
            StoreError::Rejected(rejected) => rejected.fmt(f),
            StoreError::Db(err) => write!(f, "database: {err}"),
            StoreError::Json(err) => write!(f, "stored job does not parse: {err}"),
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
        let job = serde_json::to_string(&Job::new(request))?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO jobs (project, requested_by, job) VALUES (?1, ?2, ?3)",
            params![project, requested_by, job],
        )?;
        Ok(conn.last_insert_rowid())
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
        row.job.apply(event).map_err(StoreError::Rejected)?;
        tx.execute(
            "UPDATE jobs SET job = ?1 WHERE id = ?2",
            params![serde_json::to_string(&row.job)?, id],
        )?;
        tx.commit()?;
        Ok(row)
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

    /// The Claude Code session UUID for `session` of job `job_id`, created
    /// on first use.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn session_uuid(&self, job_id: i64, session: Session) -> Result<String, StoreError> {
        let key = match session {
            Session::Planning => "planning".to_owned(),
            Session::Pr(pr) => format!("pr-{pr}"),
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
        row.job.apply(event).map_err(StoreError::Rejected)?;
        tx.execute(
            "UPDATE jobs SET job = ?1 WHERE id = ?2",
            params![serde_json::to_string(&row.job)?, job_id],
        )?;
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
