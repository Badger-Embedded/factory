//! The server's registry of the agents a stage can run on.
//!
//! [`factory_core::Agent`] names the agents and carries what any crate can
//! derive from the id alone: the display name, the available flag and the
//! effort levels. The server needs more to run and present one: the credential
//! to pass a worker, the worker image, the config directory on the host, the
//! settings page that holds the agent's account controls, and the model aliases
//! the stage panel offers. [`AgentRegistry`] maps each agent to those, so
//! [`podman`](crate::podman) and the settings pages read one place instead of
//! naming Claude.
//!
//! Claude is the one agent that runs. Its credential is the subscription token
//! the [`ClaudeToken`] resolver reads afresh on each run, its image is the
//! `worker_image` setting, and its config directory and stdout parser live in
//! `factory-runner` behind [`factory_runner::backend`]. Codex is in the registry
//! as the place the next agent goes: it has no credential, image or settings
//! page, and [`AgentEntry::credential`] and [`AgentEntry::image`] report that it
//! cannot run.

use std::sync::Arc;

use factory_core::{Agent, EFFORTS};
use factory_runner::backend;

use crate::secrets::ClaudeToken;
use crate::settings::Settings;

/// Model aliases the Claude stage panel offers. Any other model id can be typed.
const CLAUDE_MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// What the server needs for each agent, built once at startup and shared by
/// the executor and the settings pages. It holds the [`ClaudeToken`] resolver
/// so a run reads the current token; the plan usage tile keeps its own handle
/// on the same resolver and is not part of the registry.
pub struct AgentRegistry {
    claude_token: Arc<ClaudeToken>,
}

impl AgentRegistry {
    /// Builds the registry from the Claude token resolver.
    #[must_use]
    pub fn new(claude_token: Arc<ClaudeToken>) -> Self {
        AgentRegistry { claude_token }
    }

    /// The entry for one agent.
    #[must_use]
    pub fn entry(&self, agent: Agent) -> AgentEntry<'_> {
        AgentEntry {
            agent,
            registry: self,
        }
    }

    /// Every entry, in the order the dashboard lists them.
    pub fn entries(&self) -> impl Iterator<Item = AgentEntry<'_>> {
        Agent::ALL.into_iter().map(|agent| self.entry(agent))
    }
}

/// One agent's registry entry. It answers what the server needs to run the
/// agent and to draw its controls. An entry for an agent that cannot run, such
/// as Codex, reports no credential, image or settings page.
pub struct AgentEntry<'a> {
    agent: Agent,
    registry: &'a AgentRegistry,
}

impl AgentEntry<'_> {
    /// The agent this entry describes.
    #[must_use]
    pub fn agent(&self) -> Agent {
        self.agent
    }

    /// The agent's stable id.
    #[must_use]
    pub fn id(&self) -> &'static str {
        self.agent.id()
    }

    /// The name the dashboard shows.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        self.agent.display_name()
    }

    /// Whether the agent can run a stage.
    #[must_use]
    pub fn available(&self) -> bool {
        self.agent.available()
    }

    /// The settings page that holds the agent's account controls, or `None` for
    /// an agent with no page, which the sidebar lists without a link.
    #[must_use]
    pub fn settings_url(&self) -> Option<&'static str> {
        match self.agent {
            Agent::Claude => Some("/settings/agents/claude"),
            Agent::Codex => None,
        }
    }

    /// The model aliases the stage panel offers for the agent. Any other model
    /// id can be typed. An agent that cannot run offers none.
    #[must_use]
    pub fn model_suggestions(&self) -> &'static [&'static str] {
        match self.agent {
            Agent::Claude => &CLAUDE_MODELS,
            Agent::Codex => &[],
        }
    }

    /// The effort levels the stage panel offers for the agent. An agent that
    /// cannot run offers none.
    #[must_use]
    pub fn efforts(&self) -> &'static [&'static str] {
        match self.agent {
            Agent::Claude => &EFFORTS,
            Agent::Codex => &[],
        }
    }

    /// The worker image the agent runs in, or `None` for an agent that cannot
    /// run. Claude runs in the `worker_image` setting, edited on the Workers
    /// page.
    #[must_use]
    pub fn image(&self, settings: &Settings) -> Option<String> {
        match self.agent {
            Agent::Claude => Some(settings.worker_image.clone()),
            Agent::Codex => None,
        }
    }

    /// The config directory's name under the job directory on the host, where
    /// the agent's session files live, or `None` for an agent that cannot run.
    #[must_use]
    pub fn host_config_dir(&self) -> Option<&'static str> {
        backend(self.agent).map(factory_runner::AgentBackend::host_config_dir)
    }

    /// The credential a run passes to the worker, or the reason the run cannot
    /// start without it. Claude resolves the subscription token afresh and fails
    /// with "no Claude token is set" when none is stored or in the environment,
    /// so a run gates on the credential before any container starts.
    ///
    /// # Errors
    ///
    /// Returns the failure reason when no credential is available, or when the
    /// agent cannot run.
    pub fn credential(&self) -> Result<String, String> {
        match self.agent {
            Agent::Claude => self
                .registry
                .claude_token
                .resolve()
                .ok_or_else(|| "no Claude token is set".to_owned()),
            Agent::Codex => Err(format!("{} cannot run yet", self.agent.display_name())),
        }
    }
}
