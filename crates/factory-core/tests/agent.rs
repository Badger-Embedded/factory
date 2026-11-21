use factory_core::{Agent, DEFAULT_AGENT};

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
fn claude_effort_levels_and_default_are_unchanged() {
    assert_eq!(
        Agent::Claude.efforts(),
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(Agent::Claude.default_effort(), "medium");
}

#[test]
fn codex_has_its_own_effort_levels_and_default() {
    assert_eq!(
        Agent::Codex.efforts(),
        ["minimal", "low", "medium", "high", "xhigh"]
    );
    assert_eq!(Agent::Codex.default_effort(), "medium");
}

#[test]
fn every_agent_accepts_its_own_default_and_levels() {
    for agent in Agent::ALL {
        assert!(
            agent.accepts_effort(agent.default_effort()),
            "{} rejects its own default",
            agent.id()
        );
        for level in agent.efforts() {
            assert!(
                agent.accepts_effort(level),
                "{} rejects {level}",
                agent.id()
            );
        }
    }
}

#[test]
fn an_agent_refuses_a_level_another_agent_owns() {
    assert!(!Agent::Claude.accepts_effort("minimal"));
    assert!(!Agent::Codex.accepts_effort("max"));
}
