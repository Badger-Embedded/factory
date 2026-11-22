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
    assert_eq!(Agent::Copilot.id(), "copilot");
    assert_eq!(Agent::Copilot.display_name(), "Copilot");
}

#[test]
fn all_lists_the_agents_in_dashboard_order() {
    assert_eq!(Agent::ALL, [Agent::Claude, Agent::Codex, Agent::Copilot]);
}

#[test]
fn copilot_parses_from_its_id() {
    assert_eq!(Agent::from_id("copilot"), Ok(Agent::Copilot));
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
fn every_agent_can_run() {
    assert!(Agent::Claude.available());
    assert!(Agent::Codex.available());
    assert!(Agent::Copilot.available());
}

#[test]
fn each_agent_names_the_rules_file_it_reads() {
    assert_eq!(Agent::Claude.rules_doc(), "CLAUDE.md");
    assert_eq!(Agent::Codex.rules_doc(), "AGENTS.md");
    assert_eq!(Agent::Copilot.rules_doc(), "AGENTS.md");
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
fn copilot_has_its_own_effort_levels_and_default() {
    assert_eq!(
        Agent::Copilot.efforts(),
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(Agent::Copilot.default_effort(), "medium");
}

#[test]
fn an_agent_refuses_a_level_another_agent_owns() {
    assert!(!Agent::Claude.accepts_effort("minimal"));
    assert!(!Agent::Codex.accepts_effort("max"));
    assert!(Agent::Copilot.accepts_effort("max"));
    assert!(!Agent::Copilot.accepts_effort("minimal"));
}
