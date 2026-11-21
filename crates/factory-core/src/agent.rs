//! The agents a stage can run on.
//!
//! The factory runs every stage with an agent. Claude Code is the one agent
//! that runs today; Codex stands in the registry as the place the next agent
//! goes, marked unavailable until it is built. Each agent has a stable id that
//! the server stores per stage, a display name for the dashboard, and a flag
//! for whether it can run. A stage with no saved agent runs on [`DEFAULT_AGENT`].
//!
//! Each agent carries its own effort levels and default, because the CLIs do not
//! agree: Claude's `--effort` takes `low` through `max`, and Codex's
//! `model_reasoning_effort` takes `minimal` through `xhigh`. [`Agent::efforts`]
//! and [`Agent::default_effort`] expose them, so the server and the runner read
//! one list and one default per agent that cannot drift.

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
const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The effort levels the Codex agent accepts for `model_reasoning_effort`,
/// lowest first. Confirmed against the pinned Codex CLI when the Codex backend
/// lands.
const CODEX_EFFORTS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];

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

    /// The effort levels this agent accepts, lowest first. Claude's are the
    /// levels the `claude` CLI takes for `--effort`; Codex's are the levels the
    /// Codex CLI takes for `model_reasoning_effort`.
    #[must_use]
    pub fn efforts(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &CLAUDE_EFFORTS,
            Agent::Codex => &CODEX_EFFORTS,
        }
    }

    /// The effort level a stage runs at when a project saves none. Both agents
    /// default to `medium` today; the level still belongs to the agent so a
    /// future agent can differ, which is why the arms are kept apart.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn default_effort(self) -> &'static str {
        match self {
            Agent::Claude => "medium",
            Agent::Codex => "medium",
        }
    }

    /// Whether this agent accepts `effort` as one of its levels.
    #[must_use]
    pub fn accepts_effort(self, effort: &str) -> bool {
        self.efforts().contains(&effort)
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
