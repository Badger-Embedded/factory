//! Which job holds which worker slot, shared between the scheduler, which
//! fills and frees the slots, and the dashboard, which shows them.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;

/// A job in a worker slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// The slot number, from 1.
    pub number: usize,
    pub job_id: i64,
    /// When the job took the slot, in Unix seconds.
    pub since: i64,
}

/// Names one task of a job, so a task that ends late cannot remove the entry
/// of the task that replaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskId(u64);

#[derive(Debug)]
struct TaskEntry {
    id: TaskId,
    worker: bool,
    handle: Option<JoinHandle<()>>,
    stopping: bool,
}

#[derive(Debug, Default)]
struct Tasks {
    issued: u64,
    by_job: BTreeMap<i64, TaskEntry>,
}

#[derive(Debug, Default)]
pub struct WorkerSlots {
    /// Slot number to job id and start time.
    slots: Mutex<BTreeMap<usize, (i64, i64)>>,
    tasks: Mutex<Tasks>,
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

    /// Registers the task `spawn` starts for `job_id`. `spawn` runs under the
    /// registry lock, so the task cannot finish before its entry exists, and it
    /// receives the id to pass to [`Self::finish_task`]. A `worker` task counts
    /// against the worker limit.
    pub fn register_task(
        &self,
        job_id: i64,
        worker: bool,
        spawn: impl FnOnce(TaskId) -> JoinHandle<()>,
    ) {
        let mut tasks = self.tasks();
        tasks.issued += 1;
        let id = TaskId(tasks.issued);
        let handle = spawn(id);
        tasks.by_job.insert(
            job_id,
            TaskEntry {
                id,
                worker,
                handle: Some(handle),
                stopping: false,
            },
        );
    }

    /// Removes the entry of task `id` once it has finished. An entry that a
    /// stop owns, or that a newer task replaced, stays.
    pub fn finish_task(&self, job_id: i64, id: TaskId) {
        let mut tasks = self.tasks();
        if tasks
            .by_job
            .get(&job_id)
            .is_some_and(|entry| entry.id == id && !entry.stopping)
        {
            tasks.by_job.remove(&job_id);
        }
    }

    /// Whether `job_id` still has a task or a stop in progress, so its files
    /// and rows are not safe to delete.
    #[must_use]
    pub fn has_task(&self, job_id: i64) -> bool {
        self.tasks().by_job.contains_key(&job_id)
    }

    /// The jobs whose task has not ended, in id order. A stop in progress is
    /// not listed.
    #[must_use]
    pub fn running(&self) -> Vec<i64> {
        self.tasks()
            .by_job
            .iter()
            .filter(|(_, entry)| entry.is_running())
            .map(|(job_id, _)| *job_id)
            .collect()
    }

    /// How many worker tasks have not ended.
    #[must_use]
    pub fn workers_running(&self) -> usize {
        self.tasks()
            .by_job
            .values()
            .filter(|entry| entry.worker && entry.is_running())
            .count()
    }

    /// Drops the entries of tasks that ended without removing themselves.
    pub fn prune_tasks(&self) {
        self.tasks()
            .by_job
            .retain(|_, entry| entry.stopping || entry.is_running());
    }

    /// Aborts the task of `job_id` and returns its handle. The entry stays,
    /// marked as stopping, until [`Self::end_stop`], so the job keeps counting
    /// as having a task while the container stops. `None` when the job has no
    /// task to stop.
    pub fn begin_stop(&self, job_id: i64) -> Option<JoinHandle<()>> {
        let mut tasks = self.tasks();
        let entry = tasks.by_job.get_mut(&job_id)?;
        let handle = entry.handle.take()?;
        entry.stopping = true;
        handle.abort();
        Some(handle)
    }

    /// Removes the entry [`Self::begin_stop`] left.
    pub fn end_stop(&self, job_id: i64) {
        let mut tasks = self.tasks();
        if tasks
            .by_job
            .get(&job_id)
            .is_some_and(|entry| entry.stopping)
        {
            tasks.by_job.remove(&job_id);
        }
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

    fn tasks(&self) -> MutexGuard<'_, Tasks> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TaskEntry {
    fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
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
