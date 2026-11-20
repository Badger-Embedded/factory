use factory_core::{AgentResult, Ask, Event, Gates, Job, Stage, Task, Turn};

#[test]
fn stages_name_themselves_like_the_job_page() {
    let cases = [
        (Stage::Requested, "requested", None),
        (Stage::Clarifying, "clarifying", None),
        (Stage::Spec, "spec", None),
        (Stage::Plan, "plan", None),
        (Stage::Implementing { pr: 1 }, "implementing", Some(1)),
        (Stage::Verifying { pr: 2 }, "verifying", Some(2)),
        (Stage::PrOpen { pr: 0 }, "pr-open", Some(0)),
        (Stage::CiAndReview { pr: 3 }, "ci-and-review", Some(3)),
        (Stage::Merged, "merged", None),
        (Stage::Cancelled, "cancelled", None),
        (
            Stage::Failed {
                reason: "x".into(),
                retry: None,
            },
            "failed",
            None,
        ),
    ];
    for (stage, name, pr) in cases {
        assert_eq!(stage.name(), name);
        assert_eq!(stage.pr(), pr, "{name}");
    }
}

#[test]
fn a_turn_names_its_actor_and_what_it_does() {
    assert_eq!(Turn::Agent(Task::WriteSpec).name(), "agent:write-spec");
    assert_eq!(
        Turn::Agent(Task::FixChecks { report: "r".into() }).name(),
        "agent:fix-checks"
    );
    assert_eq!(Turn::User(Ask::PlanApproval).name(), "user:plan-approval");
    assert_eq!(
        Turn::User(Ask::ReviewStopped { reason: "r".into() }).name(),
        "user:review-stopped"
    );
    assert_eq!(Turn::Factory.name(), "factory");
    assert_eq!(Turn::RerunCi { reason: "r".into() }.name(), "rerun-ci");
    assert_eq!(Turn::Done.name(), "done");
}

#[test]
fn an_agent_event_names_its_result() {
    assert_eq!(Event::Start.name(), "start");
    assert_eq!(
        Event::Agent(AgentResult::Clarified).name(),
        "agent:clarified"
    );
    assert_eq!(
        Event::Agent(AgentResult::RerunCi { reason: "r".into() }).name(),
        "agent:rerun-ci"
    );
    assert_eq!(
        Event::ChecksFailed { report: "r".into() }.name(),
        "checks-failed"
    );
    assert_eq!(Event::ReviewApproved.name(), "review-approved");
    assert_eq!(Event::Retry.name(), "retry");
}

#[test]
fn a_new_job_starts_requested_with_the_factory_to_move() {
    let job = Job::new("x", Gates::default());
    assert_eq!(job.stage().name(), "requested");
    assert_eq!(job.turn().name(), "factory");
}
