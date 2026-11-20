use factory_core::{Agent, DEFAULT_AGENT, DEFAULT_EFFORT, EFFORTS};

#[test]
fn an_id_round_trips_through_parsing() {
    for agent in Agent::ALL {
        assert_eq!(Agent::from_id(agent.id()), Ok(agent), "{}", agent.id());
    }
}

#[test]
fn the_ids_and_names_are_the_ones_the_dashboard_stores_and_shows() {
    assert_eq!(Agent::Claude.id(), "claude");
    assert_eq!(Agent::Claude.display_name(), "Claude");
    assert_eq!(Agent::Codex.id(), "codex");
    assert_eq!(Agent::Codex.display_name(), "Codex");
}

#[test]
fn an_unknown_id_does_not_parse() {
    let err = Agent::from_id("gemini").unwrap_err();
    assert!(err.to_string().contains("gemini"), "{err}");
    assert!(Agent::from_id("").is_err());
    assert!(Agent::from_id("Claude").is_err());
}

#[test]
fn claude_is_the_default_agent() {
    assert_eq!(DEFAULT_AGENT, Agent::Claude);
}

#[test]
fn only_claude_can_run_today() {
    assert!(Agent::Claude.available());
    assert!(!Agent::Codex.available());
}

#[test]
fn the_effort_list_and_default_are_unchanged() {
    assert_eq!(EFFORTS, ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(DEFAULT_EFFORT, "medium");
    assert!(EFFORTS.contains(&DEFAULT_EFFORT));
}
