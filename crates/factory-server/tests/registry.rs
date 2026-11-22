//! Tests for the agent registry: the Claude and Codex entries each carry their
//! credential, worker image, config directory, settings page and stage-panel
//! suggestions. The Copilot entry is available but has no backend or login
//! wired, so its run gates on the credential.

use std::sync::Arc;

use factory_core::Agent;
use factory_server::{
    AgentRegistry, ClaudeToken, CodexLogin, LocalSecrets, MASTER_KEY_LEN, SecretStore, Settings,
};

/// A subscription `auth.json`, shaped like the one `codex login` writes.
const AUTH: &str = r#"{"tokens":{"access_token":"at-1","refresh_token":"rt-1"},"last_refresh":"2026-11-20T00:00:00Z"}"#;

fn registry_with(token: Option<&str>, codex: Option<&str>) -> AgentRegistry {
    let store = Arc::new(factory_server::Store::open_in_memory().expect("open store"));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
    if let Some(value) = token {
        secrets.set_claude_token(value).expect("store token");
    }
    if let Some(value) = codex {
        secrets.set_codex_auth(value).expect("store codex login");
    }
    AgentRegistry::new(
        Arc::new(ClaudeToken::new(Arc::clone(&secrets), None)),
        Arc::new(CodexLogin::new(secrets)),
    )
}

fn registry_with_token(token: Option<&str>) -> AgentRegistry {
    registry_with(token, None)
}

#[test]
fn the_registry_lists_the_agents_in_order() {
    let registry = registry_with_token(None);
    let ids: Vec<&str> = registry.entries().map(|entry| entry.id()).collect();
    assert_eq!(ids, ["claude", "codex", "copilot"]);
}

#[test]
fn the_copilot_entry_is_available_and_gates_on_its_credential() {
    let registry = registry_with_token(None);
    let copilot = registry.entry(Agent::Copilot);

    assert_eq!(copilot.display_name(), "Copilot");
    assert!(copilot.available());
    assert_eq!(copilot.efforts(), ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(copilot.default_effort(), "medium");

    assert_eq!(copilot.settings_url(), None);
    assert_eq!(copilot.host_config_dir(), None);
    assert_eq!(copilot.model_suggestions(), [] as [&str; 0]);
    let settings = Settings {
        worker_image: "localhost/factory-worker:latest".into(),
        ..Settings::default()
    };
    assert_eq!(copilot.image(&settings), None);

    assert_eq!(
        copilot.credential(),
        Err("no Copilot login is set".to_owned())
    );
}

#[test]
fn the_claude_entry_carries_its_image_and_suggestions() {
    let registry = registry_with_token(None);
    let claude = registry.entry(Agent::Claude);

    assert_eq!(claude.display_name(), "Claude");
    assert!(claude.available());
    assert_eq!(claude.settings_url(), Some("/settings/agents/claude"));
    assert_eq!(claude.host_config_dir(), Some("claude"));
    assert_eq!(
        claude.model_suggestions(),
        ["fable", "opus", "sonnet", "haiku"]
    );
    assert_eq!(claude.efforts(), ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(claude.default_effort(), "medium");

    let settings = Settings {
        worker_image: "localhost/factory-worker:latest".into(),
        ..Settings::default()
    };
    assert_eq!(
        claude.image(&settings),
        Some("localhost/factory-worker:latest".to_owned())
    );
}

#[test]
fn the_claude_entry_resolves_a_stored_token() {
    let with_token = registry_with_token(Some("sk-ant-oat01-stored"));
    assert_eq!(
        with_token.entry(Agent::Claude).credential(),
        Ok("sk-ant-oat01-stored".to_owned())
    );

    let without = registry_with_token(None);
    assert_eq!(
        without.entry(Agent::Claude).credential(),
        Err("no Claude token is set".to_owned())
    );
}

#[test]
fn the_codex_entry_carries_its_image_login_and_suggestions() {
    let registry = registry_with(None, Some(AUTH));
    let codex = registry.entry(Agent::Codex);

    assert_eq!(codex.display_name(), "Codex");
    assert!(codex.available());
    assert_eq!(codex.settings_url(), Some("/settings/agents/codex"));
    assert_eq!(codex.host_config_dir(), Some("codex"));
    assert_eq!(codex.model_suggestions(), ["gpt-6.1-sol", "gpt-6.1-codex"]);
    assert_eq!(
        codex.efforts(),
        ["minimal", "low", "medium", "high", "xhigh"]
    );
    assert_eq!(codex.default_effort(), "medium");

    // One worker image carries both agents, so Codex runs in `worker_image`.
    let settings = Settings {
        worker_image: "localhost/factory-worker:latest".into(),
        ..Settings::default()
    };
    assert_eq!(
        codex.image(&settings),
        Some("localhost/factory-worker:latest".to_owned())
    );

    // The stored login resolves as the credential.
    assert_eq!(codex.credential(), Ok(AUTH.to_owned()));
}

#[test]
fn the_codex_entry_fails_with_no_login() {
    let registry = registry_with(Some("sk-ant-oat01-stored"), None);
    // A Claude token stored does not stand in for a Codex login.
    assert_eq!(
        registry.entry(Agent::Codex).credential(),
        Err("no Codex login is set".to_owned())
    );
}
