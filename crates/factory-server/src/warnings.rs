//! Records an unavailable MCP server as a warning on a job, at most once per
//! server per run, and a call the proxy refused to a tool the project turned
//! off, at most once per server and tool per run. It also records each call the
//! proxy forwarded, with no dedup.
//!
//! Two paths report that a run could not use a server: the proxy, when it
//! cannot reach the server, a connection times out or the server answers 5xx;
//! and the runner, when the `mcp_servers` list in the stream-json `system`/
//! `init` line reports an active server as not `connected`. The scheduler also
//! reports a server it skipped because a stored header value would not decrypt.
//! All three funnel through [`McpWarnings`], which dedupes per run and writes
//! the warning: a factory line to the job log and an `mcp-unavailable` row to
//! `job_events`, neither of which changes the job's stage or turn.
//!
//! A refused tool call comes only from the proxy. It writes a factory line to
//! the job log and an `mcp-tool-blocked` row to `job_events`. A forwarded call
//! comes only from the proxy too and writes an `mcp-tool-call` row, with no log
//! line and no dedup, so every call the agent makes is one row.
//!
//! The dedup is keyed by job. A job runs one agent run at a time, so
//! [`McpWarnings::begin`] clears the servers warned in the job's previous run
//! before the next one starts, and a repeat report of the same server in one
//! run writes nothing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use crate::store::Store;

/// What a run already warned about: a server that was unavailable, or a
/// server and tool whose call the proxy refused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Warned {
    Unavailable(String),
    ToolBlocked(String, String),
}

/// The servers and tools already warned about in each job's current run, and
/// the store the warning is written to. Shared by the scheduler and the proxy's
/// warning callback.
pub struct McpWarnings {
    store: Arc<Store>,
    warned: Mutex<HashMap<i64, HashSet<Warned>>>,
}

impl McpWarnings {
    /// A recorder that writes to `store`.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        McpWarnings {
            store,
            warned: Mutex::new(HashMap::new()),
        }
    }

    /// Begins a run for `job_id`, forgetting the servers warned in its previous
    /// run so each run warns about a server afresh. The scheduler calls this
    /// before the run starts.
    pub fn begin(&self, job_id: i64) {
        self.lock().insert(job_id, HashSet::new());
    }

    /// Ends the run for `job_id`, dropping its dedup set. The scheduler calls
    /// this when the run's task finishes, so the map does not grow with every
    /// job the factory ever ran.
    pub fn end(&self, job_id: i64) {
        self.lock().remove(&job_id);
    }

    /// Records MCP `server` as unavailable for `job_id` with `reason`, unless
    /// this run already warned about `server`. A warning writes a job log line
    /// and an `mcp-unavailable` event; a database error is logged without the
    /// reason, which names no credential anyway.
    pub fn record(&self, job_id: i64, server: &str, reason: &str) {
        if !self.first(job_id, Warned::Unavailable(server.to_owned())) {
            return;
        }
        if let Err(err) = self.store.record_mcp_unavailable(job_id, server, reason) {
            tracing::error!("job {job_id}: recording MCP server {server:?} as unavailable: {err}");
        }
    }

    /// Records that the proxy refused `job_id`'s call to `tool` of `server`,
    /// which the project turned off, unless this run already recorded that
    /// tool. A record writes a job log line and an `mcp-tool-blocked` event.
    pub fn record_tool_blocked(&self, job_id: i64, server: &str, tool: &str) {
        if !self.first(
            job_id,
            Warned::ToolBlocked(server.to_owned(), tool.to_owned()),
        ) {
            return;
        }
        if let Err(err) = self.store.record_mcp_tool_blocked(job_id, server, tool) {
            tracing::error!("job {job_id}: recording a blocked call to {server:?} {tool:?}: {err}");
        }
    }

    /// Records that the proxy forwarded `job_id`'s call to `tool` of `server`.
    /// Unlike the blocked and unavailable warnings this is not deduped: every
    /// forwarded call writes its own `mcp-tool-call` row, because the job page
    /// counts the calls. A database error, such as an unknown job, is logged
    /// and ignored, the way the other reports are.
    pub fn record_tool_called(&self, job_id: i64, server: &str, tool: &str) {
        if let Err(err) = self.store.record_mcp_tool_call(job_id, server, tool) {
            tracing::error!("job {job_id}: recording a call to {server:?} {tool:?}: {err}");
        }
    }

    /// Whether `warning` is new for `job_id`'s run, adding it when it is.
    fn first(&self, job_id: i64, warning: Warned) -> bool {
        self.lock().entry(job_id).or_default().insert(warning)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, HashSet<Warned>>> {
        // No invariant spans the guard, so recover a poisoned lock rather than
        // fail every later warning.
        self.warned.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
