//! The agents a stage can run on.
//!
//! The factory runs every stage with an agent. Claude Code, Codex and Copilot
//! run. Local runs one of those CLIs against an OpenAI-compatible server.
//! Each agent has a stable id that the server stores per stage, a display name
//! for the dashboard, and a flag for whether a stage can select it. A stage
//! with no saved agent runs on [`DEFAULT_AGENT`].
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
    /// Codex, run with the `codex exec` CLI in a worker.
    Codex,
    /// GitHub Copilot, run with the `copilot` CLI in a worker.
    Copilot,
    /// A model on a local OpenAI-compatible server. The CLI that drives it is a
    /// runtime setting, so the agent has no fixed [`Cli`].
    Local,
}

/// A command-line tool that runs an agent in a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cli {
    /// The `claude` CLI.
    ClaudeCode,
    /// The `codex exec` CLI.
    Codex,
    /// The `copilot` CLI.
    Copilot,
}

impl Cli {
    /// Every CLI, in the order the dashboard lists them.
    pub const ALL: [Cli; 3] = [Cli::ClaudeCode, Cli::Codex, Cli::Copilot];

    /// The stable id the server stores and parses.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Cli::ClaudeCode => "claude",
            Cli::Codex => "codex",
            Cli::Copilot => "copilot",
        }
    }

    /// The name the dashboard shows.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Cli::ClaudeCode => "Claude Code",
            Cli::Codex => "Codex",
            Cli::Copilot => "Copilot",
        }
    }

    /// The CLI with this id, or `None` when no CLI has it.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Cli> {
        Cli::ALL.into_iter().find(|cli| cli.id() == id)
    }

    /// The file a target repository keeps its instructions in for this CLI.
    /// Claude Code reads `CLAUDE.md`; Codex and Copilot read `AGENTS.md`, the
    /// shared standard, and Copilot also reads `CLAUDE.md`.
    #[must_use]
    pub fn rules_doc(self) -> &'static str {
        match self {
            Cli::ClaudeCode => "CLAUDE.md",
            Cli::Codex | Cli::Copilot => "AGENTS.md",
        }
    }
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

/// The effort levels the Copilot agent accepts for `--effort`, lowest first.
/// From the pinned Copilot CLI, which lists the same levels as Claude.
const COPILOT_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The effort levels the Local agent accepts, lowest first: the levels all
/// three CLIs take.
const LOCAL_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

impl Agent {
    /// Every agent, in the order the dashboard lists them.
    pub const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::Copilot, Agent::Local];

    /// The stable id the server stores and parses. It never changes, because a
    /// rename would orphan every stored stage and session that names it.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
            Agent::Local => "local",
        }
    }

    /// The name the dashboard shows.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
            Agent::Copilot => "Copilot",
            Agent::Local => "Local",
        }
    }

    /// Whether a stage can select the agent. Every agent can run today; the flag
    /// stays so a future agent can ship listed but not selectable.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn available(self) -> bool {
        match self {
            Agent::Claude => true,
            Agent::Codex => true,
            Agent::Copilot => true,
            Agent::Local => true,
        }
    }

    /// The CLI that drives this agent in a worker, or `None` for Local, whose
    /// CLI is a setting. A caller that needs the rules file uses
    /// [`Cli::rules_doc`] on the CLI of the run.
    #[must_use]
    pub fn cli(self) -> Option<Cli> {
        match self {
            Agent::Claude => Some(Cli::ClaudeCode),
            Agent::Codex => Some(Cli::Codex),
            Agent::Copilot => Some(Cli::Copilot),
            Agent::Local => None,
        }
    }

    /// The effort levels this agent accepts, lowest first. Claude's are the
    /// levels the `claude` CLI takes for `--effort`; Codex's are the levels the
    /// Codex CLI takes for `model_reasoning_effort`; Copilot's are the levels the
    /// `copilot` CLI takes for `--effort`.
    #[must_use]
    pub fn efforts(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &CLAUDE_EFFORTS,
            Agent::Codex => &CODEX_EFFORTS,
            Agent::Copilot => &COPILOT_EFFORTS,
            Agent::Local => &LOCAL_EFFORTS,
        }
    }

    /// The effort level a stage runs at when a project saves none. Every agent
    /// defaults to `medium` today; the level still belongs to the agent so a
    /// future agent can differ, which is why the arms are kept apart.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn default_effort(self) -> &'static str {
        match self {
            Agent::Claude => "medium",
            Agent::Codex => "medium",
            Agent::Copilot => "medium",
            Agent::Local => "medium",
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
