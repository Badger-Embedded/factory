//! The agent effort level, shared by the server and the runner.
//!
//! Each stage's agent run passes `claude --effort <level>`. The server stores
//! a per-project level per stage and the runner passes it; both read the list
//! and the default from here, so they cannot drift. The default lives in code,
//! not a config file, so a project with nothing saved runs at it.

/// The effort level every stage runs at when a project saves none.
pub const DEFAULT_EFFORT: &str = "medium";

/// The levels the `claude` CLI accepts for `--effort`, lowest first. Checked
/// against CLI 2.1.329.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
