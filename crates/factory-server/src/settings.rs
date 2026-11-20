use factory_core::Gates;
use serde::{Deserialize, Serialize};

/// Factory settings that change while it runs. The job loop reads them on
/// every pass. Each field has a default, so a stored record from an older
/// version still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Worker containers that may run at the same time.
    pub max_workers: usize,
    pub worker_image: String,
    /// Passed to `podman run --memory`.
    pub worker_memory: String,
    /// Passed to `podman run --cpus`.
    pub worker_cpus: String,
    /// Seconds between looks at an open pull request, and before retrying a
    /// failed push or GitHub call.
    pub poll_seconds: u64,
    /// Squash-merge a pull request once all its checks pass and no review
    /// comment waits for the agent. Off, the user merges on GitHub.
    pub auto_merge: bool,
    /// The gates a new job starts with. A job keeps its own copy.
    pub gates: Gates,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_workers: 2,
            worker_image: "localhost/factory-worker:latest".into(),
            worker_memory: "3g".into(),
            worker_cpus: "4".into(),
            poll_seconds: 60,
            auto_merge: false,
            gates: Gates::default(),
        }
    }
}
