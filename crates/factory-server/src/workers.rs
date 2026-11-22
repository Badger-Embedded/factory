//! Which job holds which worker slot, shared between the scheduler, which
//! fills and frees the slots, and the dashboard, which shows them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// A job in a worker slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// The slot number, from 1.
    pub number: usize,
    pub job_id: i64,
    /// When the job took the slot, in Unix seconds.
    pub since: i64,
}

#[derive(Debug, Default)]
pub struct WorkerSlots {
    /// Slot number to job id and start time.
    slots: Mutex<BTreeMap<usize, (i64, i64)>>,
    /// Jobs with a scheduler task that has not ended, or a stop in progress.
    tasks: Mutex<BTreeSet<i64>>,
    pass: tokio::sync::Mutex<()>,
}

impl WorkerSlots {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts `job_id` in the lowest free slot number, or leaves it where it is
    /// when it already holds one.
    pub fn take(&self, job_id: i64, now: i64) {
        let mut slots = self.slots();
        if slots.values().any(|(id, _)| *id == job_id) {
            return;
        }
        // With n slots taken, one of 1 to n + 1 is free.
        let number = (1..=slots.len() + 1)
            .find(|n| !slots.contains_key(n))
            .unwrap_or(slots.len() + 1);
        slots.insert(number, (job_id, now));
    }

    /// Frees the slot `job_id` holds, if any.
    pub fn free(&self, job_id: i64) {
        self.slots().retain(|_, (id, _)| *id != job_id);
    }

    /// Waits for the lock the scheduler holds for a whole pass over the jobs.
    /// A caller that deletes a job holds it, so it never runs beside a pass
    /// that read the job before the deletion.
    pub async fn lock_pass(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.pass.lock().await
    }

    /// Marks `job_id` as having a task, from the spawn until the task has ended
    /// and any container stop has finished.
    pub fn begin_task(&self, job_id: i64) {
        self.tasks().insert(job_id);
    }

    /// Clears the mark [`Self::begin_task`] set.
    pub fn end_task(&self, job_id: i64) {
        self.tasks().remove(&job_id);
    }

    /// Whether `job_id` still has a task or a stop in progress, so its files
    /// and rows are not safe to delete.
    #[must_use]
    pub fn has_task(&self, job_id: i64) -> bool {
        self.tasks().contains(&job_id)
    }

    /// The busy slots, by slot number.
    #[must_use]
    pub fn busy(&self) -> Vec<Slot> {
        self.slots()
            .iter()
            .map(|(number, (job_id, since))| Slot {
                number: *number,
                job_id: *job_id,
                since: *since,
            })
            .collect()
    }

    fn slots(&self) -> MutexGuard<'_, BTreeMap<usize, (i64, i64)>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tasks(&self) -> MutexGuard<'_, BTreeSet<i64>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The current Unix time in seconds, or 0 before the epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}
