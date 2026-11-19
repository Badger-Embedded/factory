use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use factory_core::{Event, Job, Rejected};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    job TEXT NOT NULL
);
";

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
