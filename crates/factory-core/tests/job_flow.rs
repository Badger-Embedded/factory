use factory_core::{
    AgentResult, Ask, BranchSummary, Command, Event, Gates, Job, MAX_FAILED_ROUNDS, PlannedPr,
    PrDescription, Question, Rejected, Session, Stage, Task, Turn,
};

fn ready() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.".into(),
    }
}

fn planned(title: &str) -> PlannedPr {
    PlannedPr {
        title: title.into(),
        summary: format!("{title} summary"),
        number: None,
        description: None,
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
    let mut job = Job::new("add a reboot button", Gates::default());
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
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number });
    job
}

#[test]
fn request_ends_merged_after_every_planned_pull_request_merges() {
    let mut job = Job::new("add a reboot button", Gates::default());
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
    assert_runs(&job, Session::Spec, Task::WriteSpec);

    apply(&mut job, Event::Agent(AgentResult::Spec("the spec".into())));
    assert_eq!(job.turn(), &Turn::User(Ask::SpecApproval));
    assert_eq!(job.spec(), Some("the spec"));
    assert_eq!(job.next_command(), None);

    apply(&mut job, Event::Approve);
    assert_runs(&job, Session::Plan, Task::WritePlan);

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
        apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
        assert_eq!(job.plan()[index].description, Some(ready()));
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
    let mut job = Job::new("add a reboot button", Gates::default());
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
    let mut job = Job::new("add a reboot button", Gates::default());
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
fn requested_changes_go_back_to_the_stage_session() {
    let mut job = Job::new("add a reboot button", Gates::default());
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
        Session::Spec,
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
        Session::Plan,
        Task::Revise {
            feedback: "split it".into(),
        },
    );
}

#[test]
fn an_empty_plan_is_rejected() {
    let mut job = Job::new("add a reboot button", Gates::default());
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
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
        apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
        apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
        apply(
            &mut job,
            Event::ChecksFailed {
                report: "fail".into(),
            },
        );
    }
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 1 });
    apply(&mut job, Event::Merged);

    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    assert!(job.apply(Event::Merged).is_err());
    apply(&mut job, Event::ChecksPassed);
    assert!(job.apply(Event::Merged).is_err());
}

#[test]
fn merging_while_the_agent_fixes_review_comments_moves_on() {
    let mut job = implementing(&["first", "second"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
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
    let mut job = Job::new("add a reboot button", Gates::default());
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Cancel);
    assert_eq!(job.stage(), &Stage::Cancelled);
    assert_eq!(job.next_command(), None);

    assert!(job.apply(Event::Agent(AgentResult::Clarified)).is_err());
    assert!(job.apply(Event::Cancel).is_err());
}

#[test]
fn a_conflict_sends_the_agent_to_rebase_and_counts_as_a_failed_round() {
    let mut job = watching(7);
    apply(
        &mut job,
        Event::Conflicted {
            base: "main".into(),
        },
    );
    assert_runs(
        &job,
        Session::Pr(0),
        Task::Rebase {
            base: "main".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    assert_eq!(job.next_command(), Some(Command::RunChecks { pr: 0 }));

    let mut job = watching(7);
    for _ in 1..MAX_FAILED_ROUNDS {
        apply(
            &mut job,
            Event::Conflicted {
                base: "main".into(),
            },
        );
        apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
        apply(&mut job, Event::ChecksPassed);
        apply(&mut job, Event::Published { number: 7 });
    }
    apply(
        &mut job,
        Event::Conflicted {
            base: "main".into(),
        },
    );
    assert!(matches!(job.stage(), Stage::Failed { .. }));
}

#[test]
fn a_conflict_is_only_taken_while_the_factory_watches_the_pull_request() {
    let mut job = implementing(&["only"]);
    assert!(
        job.apply(Event::Conflicted {
            base: "main".into()
        })
        .is_err()
    );
}

const SKIP_SPEC: Gates = Gates {
    spec: false,
    plan: true,
    publish: false,
};
const SKIP_PLAN: Gates = Gates {
    spec: true,
    plan: false,
    publish: false,
};
const SKIP_BOTH: Gates = Gates {
    spec: false,
    plan: false,
    publish: false,
};

fn clarified(gates: Gates) -> Job {
    let mut job = Job::new("add a reboot button", gates);
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    job
}

#[test]
fn default_gates_turn_spec_and_plan_on_and_publish_off() {
    assert_eq!(
        Gates::default(),
        Gates {
            spec: true,
            plan: true,
            publish: false
        }
    );
}

#[test]
fn spec_gate_on_waits_for_approval() {
    let mut job = clarified(Gates::default());
    apply(&mut job, Event::Agent(AgentResult::Spec("the spec".into())));
    assert_eq!(job.turn(), &Turn::User(Ask::SpecApproval));
    assert_eq!(job.next_command(), None);
}

#[test]
fn spec_gate_off_goes_straight_to_the_plan() {
    let mut job = clarified(SKIP_SPEC);
    apply(&mut job, Event::Agent(AgentResult::Spec("the spec".into())));
    assert_eq!(job.spec(), Some("the spec"));
    assert_eq!(job.stage(), &Stage::Plan);
    assert_runs(&job, Session::Plan, Task::WritePlan);
}

#[test]
fn plan_gate_on_waits_for_approval() {
    let mut job = clarified(Gates::default());
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(&mut job, Event::Approve);
    apply(
        &mut job,
        Event::Agent(AgentResult::Plan(vec![planned("only")])),
    );
    assert_eq!(job.turn(), &Turn::User(Ask::PlanApproval));
    assert_eq!(job.next_command(), None);
}

#[test]
fn plan_gate_off_starts_the_first_pull_request() {
    let mut job = clarified(SKIP_PLAN);
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(&mut job, Event::Approve);
    let plan = vec![planned("refactor"), planned("feature")];
    apply(&mut job, Event::Agent(AgentResult::Plan(plan.clone())));
    assert_eq!(job.stage(), &Stage::Implementing { pr: 0 });
    assert_runs(
        &job,
        Session::Pr(0),
        Task::Implement {
            pr: plan[0].clone(),
        },
    );
}

#[test]
fn plan_gate_off_still_rejects_an_empty_plan() {
    let mut job = clarified(SKIP_PLAN);
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(&mut job, Event::Approve);
    assert_eq!(
        job.apply(Event::Agent(AgentResult::Plan(Vec::new()))),
        Err(Rejected::EmptyPlan)
    );
    assert_eq!(job.stage(), &Stage::Plan);
}

#[test]
fn a_job_with_every_gate_off_runs_to_merged_without_approval() {
    let mut job = clarified(SKIP_BOTH);
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(
        &mut job,
        Event::Agent(AgentResult::Plan(vec![planned("only")])),
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 3 });
    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
    assert_eq!(job.turn(), &Turn::Done);
}

#[test]
fn a_stored_job_without_gates_loads_with_spec_and_plan_on() {
    let stored = serde_json::to_value(Job::new("add a reboot button", SKIP_BOTH)).unwrap();
    let mut old = stored.clone();
    old.as_object_mut().unwrap().remove("gates").unwrap();

    let mut job: Job = serde_json::from_value(old).unwrap();
    assert_eq!(job, Job::new("add a reboot button", Gates::default()));

    apply(&mut job, Event::Start);
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    assert_eq!(job.turn(), &Turn::User(Ask::SpecApproval));

    let with_gates: Job = serde_json::from_value(stored).unwrap();
    assert_eq!(with_gates, Job::new("add a reboot button", SKIP_BOTH));
}

#[test]
fn a_job_reports_the_gates_it_was_created_with() {
    let gates = Gates {
        spec: false,
        plan: true,
        publish: true,
    };
    assert_eq!(Job::new("x", gates).gates(), gates);
}

fn publish_gate() -> Gates {
    Gates {
        spec: false,
        plan: false,
        publish: true,
    }
}

fn summary() -> BranchSummary {
    BranchSummary {
        commits: vec!["abc1234 maestrod: add a reboot endpoint".into()],
        diff_stat: " src/main.rs | 4 ++++\n 1 file changed, 4 insertions(+)".into(),
    }
}

/// A job with the publish gate on, whose first of `titles` has passed its
/// checks.
fn at_publish_gate(titles: &[&str]) -> Job {
    let mut job = clarified(publish_gate());
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    let plan = titles.iter().map(|t| planned(t)).collect();
    apply(&mut job, Event::Agent(AgentResult::Plan(plan)));
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    job
}

#[test]
fn publish_gate_on_stops_before_the_first_push() {
    let mut job = at_publish_gate(&["only"]);
    assert_eq!(job.stage(), &Stage::Verifying { pr: 0 });
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::PublishApproval { summary: None })
    );
    assert_eq!(job.next_command(), Some(Command::Summarize { pr: 0 }));

    apply(&mut job, Event::Summarized(summary()));
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::PublishApproval {
            summary: Some(summary())
        })
    );
    assert_eq!(job.next_command(), None);

    apply(&mut job, Event::Approve);
    assert_eq!(job.stage(), &Stage::PrOpen { pr: 0 });
    assert_eq!(job.next_command(), Some(Command::Publish { pr: 0 }));
}

#[test]
fn publish_gate_off_pushes_right_after_the_checks() {
    let mut job = clarified(Gates {
        publish: false,
        ..publish_gate()
    });
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(
        &mut job,
        Event::Agent(AgentResult::Plan(vec![planned("only")])),
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    assert_eq!(job.next_command(), Some(Command::Publish { pr: 0 }));
}

#[test]
fn requested_changes_revise_the_session_and_return_to_the_gate() {
    let mut job = at_publish_gate(&["only"]);
    apply(&mut job, Event::Summarized(summary()));
    apply(
        &mut job,
        Event::RequestChanges {
            feedback: "squash the commits".into(),
        },
    );
    assert_runs(
        &job,
        Session::Pr(0),
        Task::Revise {
            feedback: "squash the commits".into(),
        },
    );

    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    assert_eq!(job.next_command(), Some(Command::RunChecks { pr: 0 }));
    apply(&mut job, Event::ChecksPassed);
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::PublishApproval { summary: None })
    );
    assert_eq!(job.next_command(), Some(Command::Summarize { pr: 0 }));
}

#[test]
fn revisions_at_the_publish_gate_do_not_count_as_failed_rounds() {
    let mut job = at_publish_gate(&["only"]);
    for _ in 0..=MAX_FAILED_ROUNDS {
        apply(&mut job, Event::Summarized(summary()));
        apply(
            &mut job,
            Event::RequestChanges {
                feedback: "again".into(),
            },
        );
        apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
        apply(&mut job, Event::ChecksPassed);
    }
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::PublishApproval { summary: None })
    );
}

#[test]
fn only_the_first_publish_of_a_pull_request_is_gated() {
    let mut job = at_publish_gate(&["only"]);
    apply(&mut job, Event::Summarized(summary()));
    apply(&mut job, Event::Approve);
    apply(&mut job, Event::Published { number: 7 });
    apply(
        &mut job,
        Event::ReviewComments {
            comments: "rename it".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    assert_eq!(job.stage(), &Stage::PrOpen { pr: 0 });
    assert_eq!(job.next_command(), Some(Command::Publish { pr: 0 }));
}

#[test]
fn each_pull_request_of_a_plan_stops_at_the_gate() {
    let mut job = at_publish_gate(&["refactor", "feature"]);
    apply(&mut job, Event::Summarized(summary()));
    apply(&mut job, Event::Approve);
    apply(&mut job, Event::Published { number: 41 });
    apply(&mut job, Event::Merged);

    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    assert_eq!(job.stage(), &Stage::Verifying { pr: 1 });
    assert_eq!(job.next_command(), Some(Command::Summarize { pr: 1 }));
    apply(&mut job, Event::Summarized(summary()));
    apply(&mut job, Event::Approve);
    assert_eq!(job.next_command(), Some(Command::Publish { pr: 1 }));
}

#[test]
fn the_gate_takes_no_approval_before_checks_pass_and_no_summary_after() {
    let mut job = clarified(publish_gate());
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(
        &mut job,
        Event::Agent(AgentResult::Plan(vec![planned("only")])),
    );
    assert!(job.apply(Event::Approve).is_err());
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    assert!(job.apply(Event::Summarized(summary())).is_err());
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Summarized(summary()));
    assert!(job.apply(Event::Summarized(summary())).is_err());
}

#[test]
fn merged_failed_and_cancelled_jobs_are_finished() {
    let mut merged = watching(9);
    apply(&mut merged, Event::Merged);

    let mut failed = implementing(&["only"]);
    apply(
        &mut failed,
        Event::AgentFailed {
            reason: "no output".into(),
        },
    );

    let mut cancelled = implementing(&["only"]);
    apply(&mut cancelled, Event::Cancel);

    for job in [&merged, &failed, &cancelled] {
        assert!(job.is_finished(), "{:?}", job.stage());
    }
}

#[test]
fn jobs_that_still_have_a_step_are_not_finished() {
    let requested = Job::new("add a reboot button", Gates::default());
    assert!(!requested.is_finished());
    assert!(!implementing(&["only"]).is_finished());
    assert!(!watching(9).is_finished());
}

#[test]
fn spec_and_plan_each_run_in_their_own_session() {
    let mut job = Job::new("add a reboot button", Gates::default());
    assert!(job.fresh_sessions());
    apply(&mut job, Event::Start);
    assert_runs(
        &job,
        Session::Planning,
        Task::Clarify {
            request: "add a reboot button".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    assert_runs(&job, Session::Spec, Task::WriteSpec);
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(&mut job, Event::Approve);
    assert_runs(&job, Session::Plan, Task::WritePlan);
}

#[test]
fn the_job_keeps_the_answers_to_the_clarifying_questions() {
    let mut job = Job::new("add a reboot button", Gates::default());
    apply(&mut job, Event::Start);
    apply(
        &mut job,
        Event::Agent(AgentResult::Questions(vec![
            question("Which board?"),
            question("Confirm first?"),
        ])),
    );
    apply(&mut job, Event::Answer(vec!["rpi5".into(), "yes".into()]));
    let answers: Vec<(&str, &str)> = job
        .answers()
        .iter()
        .map(|a| (a.question.as_str(), a.answer.as_str()))
        .collect();
    assert_eq!(
        answers,
        [("Which board?", "rpi5"), ("Confirm first?", "yes")]
    );
}

#[test]
fn a_rejected_answer_records_nothing() {
    let mut job = Job::new("add a reboot button", Gates::default());
    apply(&mut job, Event::Start);
    apply(
        &mut job,
        Event::Agent(AgentResult::Questions(vec![question("Which board?")])),
    );
    assert!(job.apply(Event::Answer(vec![])).is_err());
    assert_eq!(job.answers(), []);
}

#[test]
fn a_stored_job_without_fresh_sessions_keeps_the_planning_session() {
    let mut stored =
        serde_json::to_value(Job::new("add a reboot button", Gates::default())).unwrap();
    let fields = stored.as_object_mut().unwrap();
    fields.remove("fresh_sessions").unwrap();
    fields.remove("answers").unwrap();

    let mut job: Job = serde_json::from_value(stored).unwrap();
    assert!(!job.fresh_sessions());
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    assert_runs(&job, Session::Planning, Task::WriteSpec);
    apply(&mut job, Event::Agent(AgentResult::Spec("spec".into())));
    apply(&mut job, Event::Approve);
    assert_runs(&job, Session::Planning, Task::WritePlan);
}
