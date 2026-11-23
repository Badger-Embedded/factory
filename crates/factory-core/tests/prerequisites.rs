use factory_core::{
    AgentResult, Event, Gates, Job, PlannedPr, PrDescription, PrerequisiteState, Stage, Wait,
};

fn job() -> Job {
    Job::new("add a reboot button", Gates::default())
}

fn failed(retryable: bool) -> Job {
    let mut job = job();
    job.apply(Event::Start).unwrap();
    if retryable {
        job.apply(Event::AgentFailed {
            reason: "usage limit".into(),
        })
        .unwrap();
        return job;
    }
    job.apply(Event::Agent(AgentResult::Clarified)).unwrap();
    job.apply(Event::Agent(AgentResult::Spec("spec".into())))
        .unwrap();
    job.apply(Event::Approve).unwrap();
    let pr = PlannedPr {
        title: "add it".into(),
        summary: "add it".into(),
        number: None,
        description: None,
        notes: None,
    };
    job.apply(Event::Agent(AgentResult::Plan(vec![pr])))
        .unwrap();
    job.apply(Event::Approve).unwrap();
    job.apply(Event::Agent(AgentResult::CommitsReady(PrDescription {
        title: "add it".into(),
        body: "body".into(),
    })))
    .unwrap();
    job.apply(Event::ChecksPassed).unwrap();
    job.apply(Event::Published { number: 9 }).unwrap();
    job.apply(Event::ClosedUnmerged).unwrap();
    job
}

fn proposal() -> Job {
    Job::proposal("Add the new build flag", Gates::default(), 7, 42)
}

fn renovate() -> Job {
    Job::renovate(
        42,
        "chore(deps): update serde",
        "https://github.com/o/r/pull/42",
    )
}

#[test]
fn the_picker_offers_unfinished_jobs_and_retryable_failures() {
    assert!(job().can_be_prerequisite());
    let mut running = job();
    running.apply(Event::Start).unwrap();
    assert!(running.can_be_prerequisite());

    let retryable = failed(true);
    assert!(matches!(retryable.stage(), Stage::Failed { .. }));
    assert!(retryable.can_retry());
    assert!(retryable.can_be_prerequisite());

    let mut dismissed = retryable.clone();
    dismissed.apply(Event::Dismiss).unwrap();
    assert!(!dismissed.can_be_prerequisite());
    let closed = failed(false);
    assert!(matches!(closed.stage(), Stage::Failed { .. }));
    assert!(!closed.can_retry());
    assert!(!closed.can_be_prerequisite());
    assert!(!renovate().can_be_prerequisite());
    assert!(!proposal().can_be_prerequisite());
}

#[test]
fn the_server_accepts_any_state_but_renovate_and_proposals() {
    assert!(failed(false).accepts_as_prerequisite());
    assert!(job().accepts_as_prerequisite());
    assert!(!renovate().accepts_as_prerequisite());
    assert!(!proposal().accepts_as_prerequisite());
}

#[test]
fn a_prerequisite_is_met_pending_or_blocked() {
    assert_eq!(job().prerequisite_state(), PrerequisiteState::Pending);
    assert_eq!(
        failed(true).prerequisite_state(),
        PrerequisiteState::Blocked
    );
    assert_eq!(
        failed(false).prerequisite_state(),
        PrerequisiteState::Blocked
    );
    let mut dismissed = failed(true);
    dismissed.apply(Event::Dismiss).unwrap();
    assert_eq!(dismissed.prerequisite_state(), PrerequisiteState::Blocked);
}

fn entry(id: i64, state: PrerequisiteState) -> (i64, PrerequisiteState, String) {
    (id, state, format!("state of {id}"))
}

#[test]
fn the_wait_folds_the_prerequisites() {
    use PrerequisiteState::{Blocked, Met, Pending};
    assert_eq!(Wait::fold([]), Wait::Ready);
    assert_eq!(Wait::fold([entry(1, Met), entry(2, Met)]), Wait::Ready);
    assert_eq!(
        Wait::fold([entry(1, Met), entry(2, Pending), entry(3, Pending)]),
        Wait::Waiting(vec![2, 3])
    );
    assert_eq!(
        Wait::fold([entry(1, Pending), entry(2, Blocked)]),
        Wait::Blocked(vec![(1, "state of 1".into()), (2, "state of 2".into())])
    );
}
