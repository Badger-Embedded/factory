use factory_core::{
    AgentResult, Ask, BranchSummary, Command, Event, Gates, Job, MAX_FAILED_ROUNDS,
    MAX_REVIEW_ROUNDS, PlannedPr, PrDescription, Question, Rejected, RetryTarget, ReviewPass,
    ReviewState, Session, Stage, Task, Turn, Verdict,
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

/// Fails the CI of the watched pull request `number` until the job runs out
/// of its failed rounds. Each round fixes nothing new, so the job keeps
/// returning to the watched pull request until the last round fails it.
fn fail_ci_until_out_of_rounds(job: &mut Job, number: u64) {
    for _ in 1..MAX_FAILED_ROUNDS {
        apply(job, Event::CiFailed { log: "fail".into() });
        apply(job, Event::Agent(AgentResult::CommitsReady(ready())));
        apply(job, Event::ChecksPassed);
        apply(job, Event::Published { number });
    }
    apply(job, Event::CiFailed { log: "last".into() });
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
fn an_agent_failure_fails_the_job_with_the_failed_task_to_retry() {
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
            reason: "usage limit".into(),
            retry: Some(RetryTarget::Agent {
                stage: Box::new(Stage::Implementing { pr: 0 }),
                task: Box::new(Task::Implement {
                    pr: planned("only")
                }),
            }),
        }
    );
    assert!(job.can_retry());
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

#[test]
fn a_rerun_request_counts_a_round_and_returns_to_watching() {
    let mut job = watching(7);
    apply(
        &mut job,
        Event::CiFailed {
            log: "flaky".into(),
        },
    );
    apply(
        &mut job,
        Event::Agent(AgentResult::RerunCi {
            reason: "network timeout".into(),
        }),
    );

    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_eq!(
        job.next_command(),
        Some(Command::RerunCi {
            pr: 0,
            number: 7,
            reason: "network timeout".into(),
        })
    );

    apply(&mut job, Event::CiRerun);
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr { pr: 0, number: 7 })
    );
}

#[test]
fn a_rerun_request_is_only_taken_from_a_fix_ci_turn() {
    let mut job = watching(7);
    let before = job.clone();
    let result = job.apply(Event::Agent(AgentResult::RerunCi {
        reason: "flaky".into(),
    }));

    assert!(result.is_err());
    assert_eq!(job, before);
}

#[test]
fn the_fifth_failed_round_fails_the_job_when_it_is_a_rerun() {
    let mut job = watching(7);
    apply(
        &mut job,
        Event::Conflicted {
            base: "main".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });
    apply(
        &mut job,
        Event::CiFailed {
            log: "flaky".into(),
        },
    );
    apply(
        &mut job,
        Event::Agent(AgentResult::RerunCi {
            reason: "flaky".into(),
        }),
    );
    apply(&mut job, Event::CiRerun);
    apply(
        &mut job,
        Event::CiFailed {
            log: "flaky".into(),
        },
    );
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });

    apply(
        &mut job,
        Event::Agent(AgentResult::RerunCi {
            reason: "flaky".into(),
        }),
    );

    assert!(matches!(job.stage(), Stage::Failed { .. }));
    assert_eq!(job.next_command(), None);
}

#[test]
fn a_retry_resumes_a_job_that_ran_out_of_ci_rounds() {
    let mut job = watching(7);
    fail_ci_until_out_of_rounds(&mut job, 7);
    assert!(matches!(job.stage(), Stage::Failed { .. }));
    assert!(job.can_retry());

    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_eq!(job.turn(), &Turn::Factory);
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr { pr: 0, number: 7 })
    );
}

#[test]
fn a_retry_resumes_a_job_that_ran_out_of_check_rounds() {
    let mut job = implementing(&["only"]);
    for _ in 0..MAX_FAILED_ROUNDS {
        apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
        apply(
            &mut job,
            Event::ChecksFailed {
                report: "fail".into(),
            },
        );
    }
    assert!(matches!(job.stage(), Stage::Failed { .. }));
    assert!(job.can_retry());

    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::Verifying { pr: 0 });
    assert_eq!(job.turn(), &Turn::Factory);
    assert_eq!(job.next_command(), Some(Command::RunChecks { pr: 0 }));
}

#[test]
fn a_retried_job_fails_again_after_five_more_rounds_and_retries_again() {
    let mut job = watching(7);
    fail_ci_until_out_of_rounds(&mut job, 7);
    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });

    // The retry reset the failed rounds, so it takes five more to fail again.
    fail_ci_until_out_of_rounds(&mut job, 7);
    assert!(matches!(job.stage(), Stage::Failed { .. }));
    assert!(job.can_retry());
}

#[test]
fn a_retry_reruns_a_failed_agent_run_in_its_session() {
    let mut job = implementing(&["only"]);
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );

    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::Implementing { pr: 0 });
    assert_runs(
        &job,
        Session::Pr(0),
        Task::Implement {
            pr: planned("only"),
        },
    );
}

#[test]
fn a_retry_reruns_a_failed_planning_run() {
    let mut job = Job::new("add a reboot button", Gates::default());
    apply(&mut job, Event::Start);
    apply(&mut job, Event::Agent(AgentResult::Clarified));
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );
    assert!(job.can_retry());

    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::Spec);
    assert_runs(&job, Session::Spec, Task::WriteSpec);
}

#[test]
fn a_retried_agent_run_keeps_the_failed_rounds() {
    // Four failed rounds, the fourth a conflict. The rebase run fails, and
    // its retry hands out no fresh rounds, so the next failure is the fifth.
    let mut job = watching(7);
    for _ in 1..MAX_FAILED_ROUNDS - 1 {
        apply(&mut job, Event::CiFailed { log: "fail".into() });
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
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );
    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_runs(
        &job,
        Session::Pr(0),
        Task::Rebase {
            base: "main".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });
    apply(
        &mut job,
        Event::CiFailed {
            log: "fifth".into(),
        },
    );
    assert!(matches!(job.stage(), Stage::Failed { .. }));
}

#[test]
fn retry_is_rejected_for_a_failure_that_cannot_resume() {
    for mut job in [closed_unmerged_job(), cancelled_job()] {
        assert!(!job.can_retry());
        let before = job.clone();
        assert!(matches!(
            job.apply(Event::Retry),
            Err(Rejected::Unexpected { .. })
        ));
        assert_eq!(job, before);
    }
}

#[test]
fn retry_is_rejected_for_a_merged_job() {
    let mut job = watching(9);
    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
    assert!(!job.can_retry());
    assert!(job.apply(Event::Retry).is_err());
}

#[test]
fn retry_is_rejected_while_the_job_still_runs() {
    let mut job = watching(9);
    assert!(!job.can_retry());
    let before = job.clone();
    assert!(job.apply(Event::Retry).is_err());
    assert_eq!(job, before);
}

fn closed_unmerged_job() -> Job {
    let mut job = watching(9);
    apply(&mut job, Event::ClosedUnmerged);
    job
}

fn cancelled_job() -> Job {
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Cancel);
    job
}

/// Drops the retry target from a failed job's JSON, so it loads like a job
/// stored before the target existed.
fn without_retry_target(job: &Job) -> Job {
    let mut value: serde_json::Value = serde_json::to_value(job).unwrap();
    value["stage"]["Failed"]
        .as_object_mut()
        .unwrap()
        .remove("retry");
    serde_json::from_value(value).unwrap()
}

#[test]
fn a_legacy_failed_job_retries_at_its_highest_numbered_pull_request() {
    let mut job = implementing(&["first", "second"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 10 });
    apply(&mut job, Event::Merged);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 11 });
    fail_ci_until_out_of_rounds(&mut job, 11);

    let mut legacy = without_retry_target(&job);
    assert!(matches!(legacy.stage(), Stage::Failed { .. }));
    assert!(legacy.can_retry());

    apply(&mut legacy, Event::Retry);
    assert_eq!(legacy.stage(), &Stage::CiAndReview { pr: 1 });
    assert_eq!(
        legacy.next_command(),
        Some(Command::WatchPr { pr: 1, number: 11 })
    );
}

/// The reason a run stopped by the plan's usage limit fails with.
const USAGE_LIMIT: &str = "the agent run ended with success: You've hit your 5-hour limit";

#[test]
fn a_legacy_agent_failure_mid_rebase_retries_at_its_pull_request() {
    let mut job = watching(75);
    apply(
        &mut job,
        Event::Conflicted {
            base: "main".into(),
        },
    );
    apply(
        &mut job,
        Event::AgentFailed {
            reason: USAGE_LIMIT.into(),
        },
    );

    let mut legacy = without_retry_target(&job);
    assert!(legacy.can_retry());
    apply(&mut legacy, Event::Retry);
    assert_eq!(legacy.stage(), &Stage::CiAndReview { pr: 0 });
    assert_eq!(
        legacy.next_command(),
        Some(Command::WatchPr { pr: 0, number: 75 })
    );
}

#[test]
fn a_legacy_agent_failure_on_an_unopened_pull_request_moves_past_the_merged_one() {
    let mut job = implementing(&["first", "second"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 74 });
    apply(&mut job, Event::Merged);
    apply(
        &mut job,
        Event::AgentFailed {
            reason: USAGE_LIMIT.into(),
        },
    );

    let mut legacy = without_retry_target(&job);
    assert!(legacy.can_retry());
    apply(&mut legacy, Event::Retry);
    assert_eq!(legacy.stage(), &Stage::CiAndReview { pr: 0 });
    // Watching finds #74 merged, so the job goes on to implement the second.
    apply(&mut legacy, Event::Merged);
    assert_runs(
        &legacy,
        Session::Pr(1),
        Task::Implement {
            pr: planned("second"),
        },
    );
}

#[test]
fn a_legacy_failure_with_another_reason_is_not_retryable() {
    let mut job = watching(9);
    apply(
        &mut job,
        Event::Conflicted {
            base: "main".into(),
        },
    );
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "could not prepare the checkout: boom".into(),
        },
    );
    let legacy = without_retry_target(&job);
    assert!(!legacy.can_retry());
}

#[test]
fn a_legacy_failed_job_with_no_pull_request_number_is_not_retryable() {
    let mut job = implementing(&["only"]);
    for _ in 0..MAX_FAILED_ROUNDS {
        apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
        apply(
            &mut job,
            Event::ChecksFailed {
                report: "fail".into(),
            },
        );
    }
    assert_eq!(job.plan()[0].number, None);

    let mut legacy = without_retry_target(&job);
    assert!(matches!(legacy.stage(), Stage::Failed { .. }));
    assert!(!legacy.can_retry());
    assert!(legacy.apply(Event::Retry).is_err());
}

/// Runs one changes-requested review round on the watched pull request
/// `number`: the reviewer asks for changes, the agent fixes and pushes, and
/// the checks pass, so the head is ready for the next review.
fn review_changes_round(job: &mut Job, number: u64) {
    apply(job, Event::StartReview);
    apply(
        job,
        Event::ReviewChanges {
            feedback: "tighten the error path".into(),
        },
    );
    apply(job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(job, Event::ChecksPassed);
    apply(job, Event::Published { number });
}

/// Runs one failed-CI round on the watched pull request `number`.
fn ci_fail_round(job: &mut Job, number: u64) {
    apply(job, Event::CiFailed { log: "fail".into() });
    apply(job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(job, Event::ChecksPassed);
    apply(job, Event::Published { number });
}

#[test]
fn an_approved_review_records_the_pass_and_returns_to_watching() {
    let mut job = watching(7);
    assert_eq!(job.review_state(), ReviewState::Pending);

    apply(&mut job, Event::StartReview);
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_runs(&job, Session::Review(0), Task::Review);

    apply(&mut job, Event::ReviewApproved);
    assert_eq!(job.review_state(), ReviewState::Approved);
    assert_eq!(job.turn(), &Turn::Factory);
    assert_eq!(
        job.reviews(),
        [ReviewPass {
            number: 1,
            verdict: Verdict::Approve,
            feedback: None,
        }]
    );
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr { pr: 0, number: 7 })
    );

    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
}

#[test]
fn requested_changes_go_to_the_implement_session_and_re_review_after_the_push() {
    let mut job = watching(7);
    apply(&mut job, Event::StartReview);
    apply(
        &mut job,
        Event::ReviewChanges {
            feedback: "rename the field".into(),
        },
    );
    assert_runs(
        &job,
        Session::Pr(0),
        Task::AddressReview {
            comments: "rename the field".into(),
        },
    );
    assert_eq!(job.review_state(), ReviewState::Pending);
    assert_eq!(
        job.reviews(),
        [ReviewPass {
            number: 1,
            verdict: Verdict::Changes,
            feedback: Some("rename the field".into()),
        }]
    );

    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });
    assert_eq!(job.review_state(), ReviewState::Pending);

    apply(&mut job, Event::StartReview);
    apply(&mut job, Event::ReviewApproved);
    assert_eq!(
        job.reviews().iter().map(|r| r.number).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(job.reviews()[1].verdict, Verdict::Approve);
}

#[test]
fn start_review_runs_only_once_until_a_new_push() {
    let mut job = watching(7);
    apply(&mut job, Event::StartReview);
    apply(&mut job, Event::ReviewApproved);
    assert_eq!(job.review_state(), ReviewState::Approved);

    let before = job.clone();
    assert!(matches!(
        job.apply(Event::StartReview),
        Err(Rejected::Unexpected { .. })
    ));
    assert_eq!(job, before);
}

#[test]
fn review_events_apply_only_in_a_review_turn() {
    let mut job = watching(7);
    assert!(job.apply(Event::ReviewApproved).is_err());
    assert!(
        job.apply(Event::ReviewChanges {
            feedback: "x".into()
        })
        .is_err()
    );
    assert!(
        job.apply(Event::ReviewFailed { reason: "x".into() })
            .is_err()
    );

    let mut implementing = implementing(&["only"]);
    assert!(implementing.apply(Event::StartReview).is_err());
}

#[test]
fn the_review_cap_stops_the_job_without_merging() {
    let mut job = watching(7);
    for _ in 1..MAX_REVIEW_ROUNDS {
        review_changes_round(&mut job, 7);
    }
    apply(&mut job, Event::StartReview);
    apply(
        &mut job,
        Event::ReviewChanges {
            feedback: "again".into(),
        },
    );

    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::ReviewStopped {
            reason: "5 review rounds on one pull request".into(),
        })
    );
    assert_eq!(job.next_command(), None);
    assert_eq!(job.reviews().len(), MAX_REVIEW_ROUNDS as usize);
    assert!(!job.is_finished());
}

#[test]
fn a_reviewer_failure_stops_the_job_without_merging() {
    let mut job = watching(7);
    apply(&mut job, Event::StartReview);
    apply(
        &mut job,
        Event::ReviewFailed {
            reason: "reviewer timed out".into(),
        },
    );
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_eq!(
        job.turn(),
        &Turn::User(Ask::ReviewStopped {
            reason: "reviewer timed out".into(),
        })
    );
    assert_eq!(job.next_command(), None);

    // The factory never merges on its own, but the user can still merge on
    // GitHub, and the factory then moves the job on.
    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
}

#[test]
fn the_review_cap_and_the_failed_round_cap_are_independent() {
    // Spend four of each cap on the same pull request. Neither counter moves
    // the other, so the job is still running.
    let mut ci_capped = watching(7);
    for _ in 1..MAX_FAILED_ROUNDS {
        ci_fail_round(&mut ci_capped, 7);
    }
    for _ in 1..MAX_REVIEW_ROUNDS {
        review_changes_round(&mut ci_capped, 7);
    }
    assert_eq!(ci_capped.stage(), &Stage::CiAndReview { pr: 0 });

    // The next CI failure is the fifth failed round, so the job fails. The
    // four review rounds did not count toward it.
    let mut review_capped = ci_capped.clone();
    apply(&mut ci_capped, Event::CiFailed { log: "fail".into() });
    assert!(matches!(ci_capped.stage(), Stage::Failed { .. }));

    // The next changes verdict is the fifth review round, so the job stops.
    // The four failed rounds did not count toward it.
    apply(&mut review_capped, Event::StartReview);
    apply(
        &mut review_capped,
        Event::ReviewChanges {
            feedback: "again".into(),
        },
    );
    assert_eq!(
        review_capped.turn(),
        &Turn::User(Ask::ReviewStopped {
            reason: "5 review rounds on one pull request".into(),
        })
    );
}

#[test]
fn the_review_cap_starts_over_for_the_next_pull_request() {
    let mut job = implementing(&["first", "second"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 1 });
    for _ in 1..MAX_REVIEW_ROUNDS {
        review_changes_round(&mut job, 1);
    }
    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Implementing { pr: 1 });

    // The second pull request gets its own five review rounds.
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 2 });
    for _ in 1..MAX_REVIEW_ROUNDS {
        review_changes_round(&mut job, 2);
    }
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 1 });
    apply(&mut job, Event::StartReview);
    apply(
        &mut job,
        Event::ReviewChanges {
            feedback: "again".into(),
        },
    );
    assert!(matches!(job.turn(), Turn::User(Ask::ReviewStopped { .. })));
}

#[test]
fn a_human_merge_moves_a_job_on_while_the_reviewer_runs() {
    let mut job = watching(7);
    apply(&mut job, Event::StartReview);
    assert_runs(&job, Session::Review(0), Task::Review);

    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
}

#[test]
fn a_review_pass_survives_a_round_trip_through_json() {
    let mut job = watching(7);
    apply(&mut job, Event::StartReview);
    apply(
        &mut job,
        Event::ReviewChanges {
            feedback: "split the function".into(),
        },
    );
    let json = serde_json::to_string(&job).unwrap();
    let back: Job = serde_json::from_str(&json).unwrap();
    assert_eq!(job, back);
}

#[test]
fn a_stored_job_without_review_fields_loads_with_no_reviews() {
    let mut stored =
        serde_json::to_value(Job::new("add a reboot button", Gates::default())).unwrap();
    let fields = stored.as_object_mut().unwrap();
    fields.remove("review_rounds").unwrap();
    fields.remove("review").unwrap();
    fields.remove("reviews").unwrap();

    let job: Job = serde_json::from_value(stored).unwrap();
    assert_eq!(job, Job::new("add a reboot button", Gates::default()));
    assert_eq!(job.review_state(), ReviewState::Pending);
    assert_eq!(job.reviews(), []);
}
