//! The agents a stage can run on.
//!
//! The factory runs every stage with an agent. Claude Code is the one agent
//! that runs today; Codex stands in the registry as the place the next agent
//! goes, marked unavailable until it is built. Each agent has a stable id that
//! the server stores per stage, a display name for the dashboard, and a flag
//! for whether it can run. A stage with no saved agent runs on [`DEFAULT_AGENT`].
//!
//! The effort levels belong to the Claude agent, which exposes them here, so
//! the server and the runner read one list and one default that cannot drift.

use std::fmt;

/// An agent that can run a stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agent {
    /// Claude Code, the one agent that runs today.
    Claude,
    /// Codex, listed in the registry but not yet built.
    Codex,
}

/// The agent a stage runs on when a project saves none.
pub const DEFAULT_AGENT: Agent = Agent::Claude;

/// The effort levels the Claude agent accepts for `--effort`, lowest first.
/// Checked against the `claude` CLI 2.1.329.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The effort level a stage runs at when a project saves none.
pub const DEFAULT_EFFORT: &str = "medium";

impl Agent {
    /// Every agent, in the order the dashboard lists them.
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    /// The stable id the server stores and parses. It never changes, because a
    /// rename would orphan every stored stage and session that names it.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// The name the dashboard shows.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
        }
    }

    /// Whether the agent can run a stage. Only Claude can today.
    #[must_use]
    pub fn available(self) -> bool {
        match self {
            Agent::Claude => true,
            Agent::Codex => false,
        }
    }

    /// The agent with this id, or [`UnknownAgent`] when no agent has it.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownAgent`] when `id` matches no agent.
    pub fn from_id(id: &str) -> Result<Agent, UnknownAgent> {
        Agent::ALL
            .into_iter()
            .find(|agent| agent.id() == id)
            .ok_or_else(|| UnknownAgent { id: id.to_owned() })
    }
}

/// An id that matches no agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAgent {
    id: String,
}

impl fmt::Display for UnknownAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown agent: {}", self.id)
    }
}

impl std::error::Error for UnknownAgent {}
