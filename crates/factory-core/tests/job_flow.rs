use factory_core::{
    AgentResult, Ask, Command, Event, Job, MAX_FAILED_ROUNDS, PlannedPr, Question, Rejected,
    Session, Stage, Task, Turn,
};

fn planned(title: &str) -> PlannedPr {
    PlannedPr {
        title: title.into(),
        summary: format!("{title} summary"),
        number: None,
    }
}

fn question(text: &str) -> Question {
    Question { text: text.into() }
}

fn apply(job: &mut Job, event: Event) {
    if let Err(rejected) = job.apply(event) {
        panic!("rejected: {rejected}");
    }
}

fn assert_runs(job: &Job, session: Session, task: Task) {
    assert_eq!(
        job.next_command(),
        Some(Command::RunAgent { session, task })
    );
}

/// A job with an approved plan of `titles`, implementing the first one.
fn implementing(titles: &[&str]) -> Job {
    let mut job = Job::new("add a reboot button");
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(&mut job, Event::Approve);
    let plan = titles.iter().map(|t| planned(t)).collect();
    apply(&mut job, Event::Agent(AgentResult::Plan(plan)));
    apply(&mut job, Event::Approve);
    job
}

/// A job whose first pull request is open as #`number` and being watched.
fn watching(number: u64) -> Job {
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number });
    job
}

#[test]
fn request_ends_merged_after_every_planned_pull_request_merges() {
    let mut job = Job::new("add a reboot button");
    assert_eq!(job.stage(), &Stage::Requested);
    assert_eq!(job.next_command(), None);

    apply(&mut job, Event::Start);
    assert_runs(
        &job,
        Session::Planning,
        Task::Clarify {
            request: "add a reboot button".into(),
        },
    );

    apply(&mut job, Event::Agent(AgentResult::Clarified));
    assert_runs(&job, Session::Planning, Task::WriteSpec);

    apply(&mut job, Event::Agent(AgentResult::Spec("the spec".into())));
    assert_eq!(job.turn(), &Turn::User(Ask::SpecApproval));
    assert_eq!(job.spec(), Some("the spec"));
    assert_eq!(job.next_command(), None);

    apply(&mut job, Event::Approve);
    assert_runs(&job, Session::Planning, Task::WritePlan);

    let plan = vec![planned("refactor"), planned("feature")];
    apply(&mut job, Event::Agent(AgentResult::Plan(plan.clone())));
    assert_eq!(job.turn(), &Turn::User(Ask::PlanApproval));
    assert_eq!(job.next_command(), None);

    apply(&mut job, Event::Approve);
    for (index, number) in [(0, 41), (1, 42)] {
        assert_runs(
            &job,
            Session::Pr(index),
            Task::Implement {
                pr: plan[index].clone(),
            },
        );
        apply(&mut job, Event::Agent(AgentResult::CommitsReady));
        assert_eq!(job.next_command(), Some(Command::RunChecks { pr: index }));
        apply(&mut job, Event::ChecksPassed);
        assert_eq!(job.next_command(), Some(Command::Publish { pr: index }));
        apply(&mut job, Event::Published { number });
        assert_eq!(job.plan()[index].number, Some(number));
        assert_eq!(
            job.next_command(),
            Some(Command::WatchPr { pr: index, number })
        );
        apply(&mut job, Event::Merged);
    }

    assert_eq!(job.stage(), &Stage::Merged);
    assert_eq!(job.turn(), &Turn::Done);
    assert_eq!(job.next_command(), None);
}

#[test]
fn answers_resume_the_session_that_asked() {
    let mut job = Job::new("add a reboot button");
    apply(&mut job, Event::Start);
    let questions = vec![question("Which board?"), question("Confirm first?")];
    apply(
        &mut job,
        Event::Agent(AgentResult::Questions(questions.clone())),
    );
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::Answers {
            questions: questions.clone()
        })
    );
    assert_eq!(job.next_command(), None);

    let answers = vec!["rpi5".to_string(), "yes".to_string()];
    apply(&mut job, Event::Answer(answers.clone()));
    assert_eq!(job.stage(), &Stage::Clarifying);
    assert_runs(&job, Session::Planning, Task::Answer { questions, answers });

    let mut job = implementing(&["only"]);
    apply(
        &mut job,
        Event::Agent(AgentResult::Questions(vec![question("Keep the old API?")])),
    );
    apply(&mut job, Event::Answer(vec!["no".into()]));
    assert_eq!(job.stage(), &Stage::Implementing { pr: 0 });
    assert!(matches!(
        job.next_command(),
        Some(Command::RunAgent {
            session: Session::Pr(0),
            task: Task::Answer { .. }
        })
    ));
}

#[test]
fn wrong_number_of_answers_is_rejected_and_leaves_the_job_unchanged() {
    let mut job = Job::new("add a reboot button");
    apply(&mut job, Event::Start);
    apply(
        &mut job,
        Event::Agent(AgentResult::Questions(vec![question("a?"), question("b?")])),
    );
    let before = job.clone();

    assert_eq!(
        job.apply(Event::Answer(vec!["only one".into()])),
        Err(Rejected::AnswerCount {
            expected: 2,
            got: 1
        })
    );
    assert_eq!(job, before);
}

#[test]
fn requested_changes_go_back_to_the_planning_session() {
    let mut job = Job::new("add a reboot button");
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    apply(&mut job, Event::Agent(AgentResult::Spec("v1".into())));
    apply(
        &mut job,
        Event::RequestChanges {
            feedback: "drop the CLI part".into(),
        },
    );
    assert_eq!(job.stage(), &Stage::Spec);
    assert_runs(
        &job,
        Session::Planning,
        Task::Revise {
            feedback: "drop the CLI part".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::Spec("v2".into())));
    assert_eq!(job.spec(), Some("v2"));
    assert_eq!(job.turn(), &Turn::User(Ask::SpecApproval));

    apply(&mut job, Event::Approve);
    apply(
        &mut job,
        Event::Agent(AgentResult::Plan(vec![planned("one")])),
    );
    apply(
        &mut job,
        Event::RequestChanges {
            feedback: "split it".into(),
        },
    );
    assert_eq!(job.stage(), &Stage::Plan);
    assert_runs(
        &job,
        Session::Planning,
        Task::Revise {
            feedback: "split it".into(),
        },
    );
}

#[test]
fn an_empty_plan_is_rejected() {
    let mut job = Job::new("add a reboot button");
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(&mut job, Event::Approve);
    let before = job.clone();

    assert_eq!(
        job.apply(Event::Agent(AgentResult::Plan(vec![]))),
        Err(Rejected::EmptyPlan)
    );
    assert_eq!(job, before);
}

#[test]
fn failed_checks_go_back_to_the_agent_and_rerun() {
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    apply(
        &mut job,
        Event::ChecksFailed {
            report: "clippy: unused variable".into(),
        },
    );
    assert_runs(
        &job,
        Session::Pr(0),
        Task::FixChecks {
            report: "clippy: unused variable".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    assert_eq!(job.next_command(), Some(Command::RunChecks { pr: 0 }));
}

#[test]
fn ci_failures_and_review_comments_are_fixed_and_pushed_to_the_same_pull_request() {
    let mut job = watching(7);

    apply(
        &mut job,
        Event::CiFailed {
            log: "test failed".into(),
        },
    );
    assert_runs(
        &job,
        Session::Pr(0),
        Task::FixCi {
            log: "test failed".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    apply(&mut job, Event::ChecksPassed);
    assert_eq!(job.next_command(), Some(Command::Publish { pr: 0 }));
    apply(&mut job, Event::Published { number: 7 });

    apply(
        &mut job,
        Event::ReviewComments {
            comments: "rename this".into(),
        },
    );
    assert_runs(
        &job,
        Session::Pr(0),
        Task::AddressReview {
            comments: "rename this".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    assert_eq!(job.stage(), &Stage::Verifying { pr: 0 });
}

#[test]
fn too_many_failed_rounds_fail_the_job() {
    let mut job = watching(7);
    for round in 1..MAX_FAILED_ROUNDS {
        apply(
            &mut job,
            Event::CiFailed {
                log: format!("round {round}"),
            },
        );
        apply(&mut job, Event::Agent(AgentResult::CommitsReady));
        apply(&mut job, Event::ChecksPassed);
        apply(&mut job, Event::Published { number: 7 });
    }
    apply(
        &mut job,
        Event::CiFailed {
            log: "again".into(),
        },
    );

    assert!(matches!(job.stage(), Stage::Failed { .. }));
    assert_eq!(job.next_command(), None);
}

#[test]
fn failed_rounds_start_over_for_the_next_pull_request() {
    let mut job = implementing(&["first", "second"]);
    for _ in 1..MAX_FAILED_ROUNDS {
        apply(&mut job, Event::Agent(AgentResult::CommitsReady));
        apply(
            &mut job,
            Event::ChecksFailed {
                report: "fail".into(),
            },
        );
    }
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 1 });
    apply(&mut job, Event::Merged);

    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    apply(
        &mut job,
        Event::ChecksFailed {
            report: "fail".into(),
        },
    );
    assert_eq!(job.stage(), &Stage::Verifying { pr: 1 });
}

#[test]
fn a_duplicate_event_is_rejected_and_leaves_the_job_unchanged() {
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    apply(&mut job, Event::ChecksPassed);
    let before = job.clone();

    assert!(matches!(
        job.apply(Event::ChecksPassed),
        Err(Rejected::Unexpected { .. })
    ));
    assert_eq!(job, before);
}

#[test]
fn merge_is_rejected_before_the_pull_request_exists() {
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    assert!(job.apply(Event::Merged).is_err());
    apply(&mut job, Event::ChecksPassed);
    assert!(job.apply(Event::Merged).is_err());
}

#[test]
fn merging_while_the_agent_fixes_review_comments_moves_on() {
    let mut job = implementing(&["first", "second"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 3 });
    apply(
        &mut job,
        Event::ReviewComments {
            comments: "nit".into(),
        },
    );

    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Implementing { pr: 1 });
}

#[test]
fn closing_the_pull_request_unmerged_fails_the_job() {
    let mut job = watching(9);
    apply(&mut job, Event::ClosedUnmerged);
    assert!(matches!(job.stage(), Stage::Failed { .. }));
}

#[test]
fn an_agent_failure_fails_the_job() {
    let mut job = implementing(&["only"]);
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );
    assert_eq!(
        job.stage(),
        &Stage::Failed {
            reason: "usage limit".into()
        }
    );
}

#[test]
fn cancel_ends_the_job_and_later_events_are_rejected() {
    let mut job = Job::new("add a reboot button");
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Cancel);
    assert_eq!(job.stage(), &Stage::Cancelled);
    assert_eq!(job.next_command(), None);

    assert!(job.apply(Event::Agent(AgentResult::Clarified)).is_err());
    assert!(job.apply(Event::Cancel).is_err());
}
