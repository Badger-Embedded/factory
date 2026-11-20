//! Which job holds which worker slot, shared between the scheduler, which
//! fills and frees the slots, and the dashboard, which shows them.

use std::collections::BTreeMap;
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
}

/// The current Unix time in seconds, or 0 before the epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}
