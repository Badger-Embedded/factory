//! Tests for the agent registry: the Claude and Codex entries each carry their
//! credential, worker image, config directory, settings page and stage-panel
//! suggestions. The Copilot entry reads its token afresh from the secret store
//! and gates the run on it. It has no settings page yet, so the sidebar lists
//! it without a link.

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
        Arc::new(CodexLogin::new(Arc::clone(&secrets))),
        secrets,
    )
}

fn registry_with_token(token: Option<&str>) -> AgentRegistry {
    registry_with(token, None)
}

#[test]
fn the_registry_lists_the_agents_in_order() {
    let registry = registry_with_token(None);
    let ids: Vec<&str> = registry.entries().map(|entry| entry.id()).collect();
    assert_eq!(ids, ["claude", "codex", "copilot", "local"]);
}

#[test]
fn the_copilot_entry_carries_its_image_suggestions_and_stored_token() {
    let store = Arc::new(factory_server::Store::open_in_memory().expect("open store"));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalSecrets::new(&[1; MASTER_KEY_LEN], Arc::clone(&store)));
    let registry = AgentRegistry::new(
        Arc::new(ClaudeToken::new(Arc::clone(&secrets), None)),
        Arc::new(CodexLogin::new(Arc::clone(&secrets))),
        Arc::clone(&secrets),
    );
    let copilot = registry.entry(Agent::Copilot);

    assert_eq!(copilot.display_name(), "Copilot");
    assert!(copilot.available());
    assert_eq!(copilot.efforts(), ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(copilot.default_effort(), "medium");
    assert_eq!(copilot.host_config_dir(), Some("copilot"));
    assert_eq!(
        copilot.model_suggestions(),
        [
            "claude-sonnet-5.5",
            "claude-opus-5.5",
            "gpt-6.2-sol",
            "gemini-3.8-flash"
        ]
    );
    let settings = Settings {
        worker_image: "localhost/factory-worker:latest".into(),
        ..Settings::default()
    };
    assert_eq!(
        copilot.image(&settings),
        Some("localhost/factory-worker:latest".to_owned())
    );

    // No token stored: the run fails on the credential.
    assert_eq!(
        copilot.credential(),
        Err("no Copilot login is set".to_owned())
    );
    // A token saved afterwards reaches the next run without a restart.
    secrets.set_copilot_token("gho_abc").expect("store token");
    assert_eq!(copilot.credential(), Ok("gho_abc".to_owned()));
    secrets.clear_copilot_token().expect("clear token");
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

#[test]
fn the_local_entry_points_at_its_page_and_offers_no_models_yet() {
    let registry = registry_with_token(None);
    let entry = registry
        .entries()
        .find(|entry| entry.id() == "local")
        .expect("local entry");
    assert_eq!(entry.display_name(), "Local");
    assert!(entry.available());
    assert_eq!(entry.settings_url(), Some("/settings/agents/local"));
    assert_eq!(entry.model_suggestions(), [] as [&str; 0]);
    assert_eq!(entry.host_config_dir(), None);
    assert!(entry.credential().is_err());
}
