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
//! All three agents run. Claude's credential is the subscription token the
//! [`ClaudeToken`] resolver reads afresh on each run; Codex's is the stored
//! `auth.json` the [`CodexLogin`] coordinator resolves; Copilot's is the GitHub
//! token read afresh from the secret store. All run in the
//! `worker_image` setting, since one image carries them, and each config
//! directory and stdout parser lives in `factory-runner` behind
//! [`factory_runner::backend`]. The registry also hands the executor the
//! [`CodexLogin`] coordinator, which keeps the login Codex refreshes mid-run.

use std::sync::Arc;

use factory_core::Agent;
use factory_runner::backend;

use crate::codex::CodexLogin;
use crate::secrets::{ClaudeToken, SecretStore};
use crate::settings::Settings;

/// Model aliases the Claude stage panel offers. Any other model id can be typed.
const CLAUDE_MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// Model aliases the Codex stage panel offers. The pinned Codex CLI
/// (`rust-v0.165.0`) defaults to the `gpt-6.1-sol` family, so the suggestions
/// track it; any other model id can be typed.
const CODEX_MODELS: [&str; 2] = ["gpt-6.1-sol", "gpt-6.1-codex"];

/// Model ids the Copilot stage panel offers, from the pinned CLI's list. Any
/// other model id can be typed.
const COPILOT_MODELS: [&str; 4] = [
    "claude-sonnet-5.5",
    "claude-opus-5.5",
    "gpt-6.2-sol",
    "gemini-3.8-flash",
];

/// What the server needs for each agent, built once at startup and shared by
/// the executor and the settings pages. It holds the [`ClaudeToken`] resolver
/// and the secret store so a run reads the current token.
pub struct AgentRegistry {
    claude_token: Arc<ClaudeToken>,
    codex_login: Arc<CodexLogin>,
    secrets: Arc<dyn SecretStore>,
}

impl AgentRegistry {
    /// Builds the registry from the Claude token resolver, the Codex login
    /// coordinator and the secret store that holds the Copilot token.
    #[must_use]
    pub fn new(
        claude_token: Arc<ClaudeToken>,
        codex_login: Arc<CodexLogin>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        AgentRegistry {
            claude_token,
            codex_login,
            secrets,
        }
    }

    /// The Codex login coordinator, which the executor uses to serialise runs
    /// while the stored login is due for a refresh and to keep the file Codex
    /// rewrites mid-run.
    #[must_use]
    pub fn codex_login(&self) -> &Arc<CodexLogin> {
        &self.codex_login
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
/// agent and to draw its controls: the credential, the worker image, the host
/// config directory, the settings page and the stage-panel suggestions.
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
            Agent::Codex => Some("/settings/agents/codex"),
            Agent::Copilot => Some("/settings/agents/copilot"),
        }
    }

    /// The model aliases the stage panel offers for the agent. Any other model
    /// id can be typed. An agent that cannot run offers none.
    #[must_use]
    pub fn model_suggestions(&self) -> &'static [&'static str] {
        match self.agent {
            Agent::Claude => &CLAUDE_MODELS,
            Agent::Codex => &CODEX_MODELS,
            Agent::Copilot => &COPILOT_MODELS,
        }
    }

    /// The effort levels the stage panel offers for the agent, lowest first.
    #[must_use]
    pub fn efforts(&self) -> &'static [&'static str] {
        self.agent.efforts()
    }

    /// The effort level the stage panel preselects for the agent when a project
    /// saved none.
    #[must_use]
    pub fn default_effort(&self) -> &'static str {
        self.agent.default_effort()
    }

    /// The worker image the agent runs in, or `None` for an agent that cannot
    /// run. Claude runs in the `worker_image` setting, edited on the Workers
    /// page.
    #[must_use]
    pub fn image(&self, settings: &Settings) -> Option<String> {
        match self.agent {
            // One worker image carries the agents that have a backend, so they
            // run in the `worker_image` setting edited on the Workers page.
            Agent::Claude | Agent::Codex | Agent::Copilot => Some(settings.worker_image.clone()),
        }
    }

    /// The config directory's name under the job directory on the host, where
    /// the agent's session files live, or `None` for an agent with no backend.
    #[must_use]
    pub fn host_config_dir(&self) -> Option<&'static str> {
        backend(self.agent).map(factory_runner::AgentBackend::host_config_dir)
    }

    /// The credential a run passes to the worker, or the reason the run cannot
    /// start without it. Claude resolves the subscription token afresh and fails
    /// with "no Claude token is set" when none is stored or in the environment;
    /// Codex resolves the stored `auth.json` and fails with "no Codex login is
    /// set"; Copilot resolves the stored token afresh and fails with "no Copilot
    /// login is set". A run gates on the credential before any container starts, so a
    /// token or login saved on the settings page reaches the next run.
    ///
    /// # Errors
    ///
    /// Returns the failure reason when no credential is available.
    pub fn credential(&self) -> Result<String, String> {
        match self.agent {
            Agent::Claude => self
                .registry
                .claude_token
                .resolve()
                .ok_or_else(|| "no Claude token is set".to_owned()),
            Agent::Codex => self.registry.codex_login.credential(),
            Agent::Copilot => self.copilot_token(),
        }
    }

    /// The stored Copilot token. An unavailable store, an empty row and a row
    /// that will not decrypt all read as no login; the last is logged without
    /// the value.
    fn copilot_token(&self) -> Result<String, String> {
        const NONE: &str = "no Copilot login is set";
        if !self.registry.secrets.available() {
            return Err(NONE.to_owned());
        }
        match self.registry.secrets.copilot_token() {
            Ok(token) => token.ok_or_else(|| NONE.to_owned()),
            Err(err) => {
                tracing::warn!("reading the stored Copilot token failed: {err}");
                Err(NONE.to_owned())
            }
        }
    }
}
