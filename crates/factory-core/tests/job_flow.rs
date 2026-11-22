use factory_core::{
    AgentResult, Ask, BranchSummary, Command, CommentKind, DiffVerdict, Event, Gates, Job, JobKind,
    MAX_FAILED_ROUNDS, MAX_REVIEW_ROUNDS, PlannedPr, PrDescription, PrDiff, Question, Rejected,
    RenovateCi, RenovateWait, RetryLimits, RetryTarget, ReviewPass, ReviewState, ReviewStop,
    Session, Stage, Task, Turn, Verdict,
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
        notes: None,
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
            Some(Command::WatchPr {
                pr: index,
                number,
                watch_only: false,
            })
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
fn a_pull_request_with_nothing_to_publish_is_skipped() {
    let mut job = implementing(&["first", "second", "third"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });
    apply(&mut job, Event::Merged);

    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    assert_eq!(job.next_command(), Some(Command::Publish { pr: 1 }));
    apply(&mut job, Event::NothingToPublish);
    assert!(job.is_skipped(1));
    assert!(!job.is_skipped(0));
    assert_eq!(job.plan()[1].number, None);
    assert_runs(
        &job,
        Session::Pr(2),
        Task::Implement {
            pr: planned("third"),
        },
    );
}

#[test]
fn skipping_the_last_pull_request_finishes_the_job() {
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::NothingToPublish);
    assert!(job.is_skipped(0));
    assert_eq!(job.stage(), &Stage::Merged);
    assert_eq!(job.turn(), &Turn::Done);
}

#[test]
fn nothing_to_publish_on_an_open_pull_request_fails_the_job() {
    let mut job = watching(9);
    apply(
        &mut job,
        Event::ReviewComments {
            comments: "nit".into(),
        },
    );
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    assert_eq!(job.stage(), &Stage::PrOpen { pr: 0 });

    apply(&mut job, Event::NothingToPublish);
    assert!(!job.is_skipped(0));
    let Stage::Failed { reason, retry, .. } = job.stage() else {
        panic!("{:?}", job.stage());
    };
    assert!(reason.contains("#9"), "{reason}");
    assert_eq!(retry, &None);
}

#[test]
fn nothing_to_publish_outside_pr_open_is_rejected() {
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    let before = job.clone();
    assert!(job.apply(Event::NothingToPublish).is_err());
    assert_eq!(job, before);
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
            dismissed: false,
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
fn editing_a_queued_request_replaces_the_text_the_clarify_run_gets() {
    let mut job = Job::new("add a reboot button", Gates::default());
    apply(
        &mut job,
        Event::Edit {
            request: "add a reboot button to the device page".into(),
        },
    );
    assert_eq!(job.stage(), &Stage::Requested);
    assert_eq!(job.turn(), &Turn::Factory);
    assert_eq!(job.request(), "add a reboot button to the device page");

    apply(&mut job, Event::Start);
    assert_runs(
        &job,
        Session::Planning,
        Task::Clarify {
            request: "add a reboot button to the device page".into(),
        },
    );
}

#[test]
fn editing_is_rejected_once_the_job_leaves_requested() {
    let mut clarifying = Job::new("add a reboot button", Gates::default());
    apply(&mut clarifying, Event::Start);
    assert_eq!(clarifying.stage(), &Stage::Clarifying);

    let implementing = implementing(&["only"]);

    let mut failed = implementing.clone();
    apply(
        &mut failed,
        Event::AgentFailed {
            reason: "no output".into(),
        },
    );

    let mut cancelled = implementing.clone();
    apply(&mut cancelled, Event::Cancel);

    for job in [&clarifying, &implementing, &failed, &cancelled] {
        let mut edited = job.clone();
        assert!(
            matches!(
                edited.apply(Event::Edit {
                    request: "changed".into(),
                }),
                Err(Rejected::Unexpected { .. })
            ),
            "edit accepted in {:?}",
            job.stage(),
        );
        assert_eq!(&edited, job, "edit changed the job in {:?}", job.stage());
    }
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
        Some(Command::WatchPr {
            pr: 0,
            number: 7,
            watch_only: false,
        })
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
        Some(Command::WatchPr {
            pr: 0,
            number: 7,
            watch_only: false,
        })
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

#[test]
fn dismiss_marks_a_retryable_failed_job_without_moving_it() {
    let mut job = implementing(&["only"]);
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );
    let before = job.clone();
    assert!(job.can_retry());
    assert!(!job.is_dismissed());

    apply(&mut job, Event::Dismiss);
    assert!(job.is_dismissed());
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
            dismissed: true,
        }
    );
    assert_eq!(job.turn(), before.turn());
    assert!(job.can_retry());
}

#[test]
fn dismiss_marks_a_failed_job_that_cannot_retry() {
    let mut job = closed_unmerged_job();
    assert!(!job.can_retry());

    apply(&mut job, Event::Dismiss);
    assert!(job.is_dismissed());
    assert!(!job.can_retry());
    assert!(matches!(job.stage(), Stage::Failed { .. }));
}

#[test]
fn dismiss_is_rejected_on_an_already_dismissed_job() {
    let mut job = closed_unmerged_job();
    apply(&mut job, Event::Dismiss);
    let before = job.clone();

    assert!(matches!(
        job.apply(Event::Dismiss),
        Err(Rejected::Unexpected { .. })
    ));
    assert_eq!(job, before);
}

#[test]
fn dismiss_is_rejected_on_merged_cancelled_and_live_jobs() {
    let mut merged = watching(9);
    apply(&mut merged, Event::Merged);

    for mut job in [
        merged,
        cancelled_job(),
        implementing(&["only"]),
        watching(9),
    ] {
        let before = job.clone();
        assert!(matches!(
            job.apply(Event::Dismiss),
            Err(Rejected::Unexpected { .. })
        ));
        assert_eq!(job, before, "dismiss changed {:?}", before.stage());
    }
}

#[test]
fn retry_clears_the_dismissal_and_a_later_failure_is_not_dismissed() {
    let mut job = implementing(&["only"]);
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );
    apply(&mut job, Event::Dismiss);
    assert!(job.is_dismissed());

    apply(&mut job, Event::Retry);
    assert!(!job.is_dismissed());
    assert_eq!(job.stage(), &Stage::Implementing { pr: 0 });

    apply(
        &mut job,
        Event::AgentFailed {
            reason: "usage limit".into(),
        },
    );
    assert!(matches!(job.stage(), Stage::Failed { .. }));
    assert!(!job.is_dismissed());
}

#[test]
fn a_stored_failed_job_without_the_dismissed_field_loads_as_not_dismissed() {
    let mut job = closed_unmerged_job();
    assert!(matches!(job.stage(), Stage::Failed { .. }));
    let mut value: serde_json::Value = serde_json::to_value(&job).unwrap();
    value["stage"]["Failed"]
        .as_object_mut()
        .unwrap()
        .remove("dismissed")
        .unwrap();

    let loaded: Job = serde_json::from_value(value).unwrap();
    assert_eq!(loaded, job);
    assert!(!loaded.is_dismissed());

    apply(&mut job, Event::Dismiss);
    assert!(job.is_dismissed());
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
        Some(Command::WatchPr {
            pr: 1,
            number: 11,
            watch_only: false,
        })
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
        Some(Command::WatchPr {
            pr: 0,
            number: 75,
            watch_only: false,
        })
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
    apply(job, start_review());
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

    apply(&mut job, start_review());
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
            pr: Some(0),
            head: job.reviews()[0].head.clone(),
            answers_seen: 0,
        }]
    );
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr {
            pr: 0,
            number: 7,
            watch_only: false,
        })
    );

    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
}

#[test]
fn requested_changes_go_to_the_implement_session_and_re_review_after_the_push() {
    let mut job = watching(7);
    apply(&mut job, start_review());
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
            pr: Some(0),
            head: job.reviews()[0].head.clone(),
            answers_seen: 0,
        }]
    );

    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });
    assert_eq!(job.review_state(), ReviewState::Pending);

    apply(&mut job, start_review());
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
    apply(&mut job, start_review());
    apply(&mut job, Event::ReviewApproved);
    assert_eq!(job.review_state(), ReviewState::Approved);

    let before = job.clone();
    assert!(matches!(
        job.apply(start_review()),
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
    assert!(implementing.apply(start_review()).is_err());
}

#[test]
fn the_review_cap_stops_the_job_without_merging() {
    let mut job = watching(7);
    for _ in 1..MAX_REVIEW_ROUNDS {
        review_changes_round(&mut job, 7);
    }
    apply(&mut job, start_review());
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
            stop: ReviewStop::CapReached {
                feedback: "again".into(),
            },
        })
    );
    // The review stopped, so the factory keeps watching the pull request for a
    // merge or close by the user, without acting on it.
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr {
            pr: 0,
            number: 7,
            watch_only: true,
        })
    );
    assert_eq!(job.reviews().len(), MAX_REVIEW_ROUNDS as usize);
    assert!(!job.is_finished());
}

fn start_review_on(head: &str) -> Event {
    Event::StartReview { head: head.into() }
}

/// Asks for changes on `head`, then the agent ends its review fix with no new
/// commit, so CI goes green on the same head again.
fn changes_without_a_commit(job: &mut Job, head: &str) {
    apply(job, start_review_on(head));
    apply(
        job,
        Event::ReviewChanges {
            feedback: "read the old variable as a fallback".into(),
        },
    );
    apply(job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(job, Event::ChecksPassed);
    apply(job, Event::Published { number: 7 });
}

#[test]
fn a_pass_records_its_pull_request_head_and_the_answers_it_saw() {
    let mut job = watching(7);
    apply(&mut job, start_review_on("abc123"));
    apply(&mut job, Event::ReviewApproved);
    let pass = &job.reviews()[0];
    assert_eq!(pass.pr, Some(0));
    assert_eq!(pass.head.as_deref(), Some("abc123"));
    assert_eq!(pass.answers_seen, job.answers().len());
    assert_eq!(job.reviews_of(0).count(), 1);
    assert_eq!(job.reviews_of(1).count(), 0);
}

#[test]
fn the_reviewer_does_not_rerun_on_the_head_it_asked_to_change() {
    let mut job = watching(7);
    changes_without_a_commit(&mut job, "abc123");

    apply(&mut job, start_review_on("abc123"));
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    let Turn::User(Ask::ReviewStopped { stop, .. }) = job.turn() else {
        panic!("expected a stopped review, got {:?}", job.turn());
    };
    assert_eq!(stop, &ReviewStop::Unchanged);
    // No pass ran, so none is recorded and no review round is spent.
    assert_eq!(job.reviews().len(), 1);
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr {
            pr: 0,
            number: 7,
            watch_only: true,
        })
    );

    // Retry runs the reviewer on that head anyway.
    assert!(job.can_retry());
    apply(&mut job, Event::Retry);
    assert_runs(&job, Session::Review(0), Task::Review);
    apply(&mut job, Event::ReviewApproved);
    assert_eq!(job.reviews()[1].head.as_deref(), Some("abc123"));
}

#[test]
fn the_reviewer_reruns_on_the_same_head_after_a_new_answer() {
    let mut job = watching(7);
    apply(&mut job, start_review_on("abc123"));
    apply(
        &mut job,
        Event::ReviewChanges {
            feedback: "read the old variable as a fallback".into(),
        },
    );
    // The agent asks the requester instead of changing the code.
    apply(
        &mut job,
        Event::Agent(AgentResult::Questions(vec![Question {
            text: "Keep the spec?".into(),
        }])),
    );
    apply(&mut job, Event::Answer(vec!["yes".into()]));
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });

    apply(&mut job, start_review_on("abc123"));
    assert_runs(&job, Session::Review(0), Task::Review);
}

#[test]
fn the_reviewer_reruns_on_a_new_head() {
    let mut job = watching(7);
    changes_without_a_commit(&mut job, "abc123");
    apply(&mut job, start_review_on("def456"));
    assert_runs(&job, Session::Review(0), Task::Review);
}

#[test]
fn a_reviewer_failure_stops_the_job_without_merging() {
    let mut job = watching(7);
    apply(&mut job, start_review());
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
            stop: ReviewStop::ReviewerFailed,
        })
    );
    // The review stopped, so the factory keeps watching the pull request. The
    // watch never merges or restarts the reviewer; it only reports a merge or
    // close by the user.
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr {
            pr: 0,
            number: 7,
            watch_only: true,
        })
    );

    // The user merges on GitHub, the watch reports it, and the factory moves
    // the job on.
    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
}

#[test]
fn a_stopped_review_watches_until_the_next_pull_request() {
    let mut job = implementing(&["first", "second"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 11 });
    apply(&mut job, start_review());
    apply(
        &mut job,
        Event::ReviewFailed {
            reason: "reviewer crashed".into(),
        },
    );
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr {
            pr: 0,
            number: 11,
            watch_only: true,
        })
    );

    // Merging the first pull request moves the job on to the second.
    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Implementing { pr: 1 });
}

#[test]
fn a_stopped_review_fails_when_the_pull_request_is_closed() {
    let mut job = watching(7);
    apply(&mut job, start_review());
    apply(
        &mut job,
        Event::ReviewFailed {
            reason: "reviewer timed out".into(),
        },
    );
    assert_eq!(
        job.next_command(),
        Some(Command::WatchPr {
            pr: 0,
            number: 7,
            watch_only: true,
        })
    );

    apply(&mut job, Event::ClosedUnmerged);
    assert!(matches!(
        job.stage(),
        Stage::Failed { reason, .. }
            if reason == "the pull request was closed without merging"
    ));
}

/// The job's review rounds on the current pull request, read back from its
/// JSON since the state machine keeps the counter private.
fn review_rounds(job: &Job) -> u64 {
    serde_json::to_value(job).unwrap()["review_rounds"]
        .as_u64()
        .unwrap()
}

/// A job stopped at the review cap on the watched pull request `number`, its
/// last pass asking for `feedback`.
fn capped_review(number: u64, feedback: &str) -> Job {
    let mut job = watching(number);
    for _ in 1..MAX_REVIEW_ROUNDS {
        review_changes_round(&mut job, number);
    }
    apply(&mut job, start_review());
    apply(
        &mut job,
        Event::ReviewChanges {
            feedback: feedback.into(),
        },
    );
    assert!(matches!(job.turn(), Turn::User(Ask::ReviewStopped { .. })));
    job
}

#[test]
fn a_retry_after_a_reviewer_failure_reruns_the_reviewer() {
    let mut job = watching(7);
    // Fail one review round first, so the kept counts are not zero.
    review_changes_round(&mut job, 7);
    apply(&mut job, start_review());
    apply(
        &mut job,
        Event::ReviewFailed {
            reason: "reviewer timed out".into(),
        },
    );
    assert!(job.can_retry());
    let rounds = review_rounds(&job);

    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_runs(&job, Session::Review(0), Task::Review);
    // The reviewer reruns with the review and failed round counts unchanged.
    assert_eq!(review_rounds(&job), rounds);
}

#[test]
fn a_retry_at_the_review_cap_addresses_the_last_feedback_with_fresh_rounds() {
    let mut job = capped_review(7, "tighten the error path");
    assert_eq!(review_rounds(&job), u64::from(MAX_REVIEW_ROUNDS));

    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    assert_runs(
        &job,
        Session::Pr(0),
        Task::AddressReview {
            comments: "tighten the error path".into(),
        },
    );
    // The review rounds are cleared, so the pull request gets five fresh ones.
    assert_eq!(review_rounds(&job), 0);

    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });
    for _ in 1..MAX_REVIEW_ROUNDS {
        review_changes_round(&mut job, 7);
    }
    assert_eq!(job.stage(), &Stage::CiAndReview { pr: 0 });
    apply(&mut job, start_review());
    apply(
        &mut job,
        Event::ReviewChanges {
            feedback: "again".into(),
        },
    );
    assert!(matches!(job.turn(), Turn::User(Ask::ReviewStopped { .. })));
}

#[test]
fn a_stopped_review_with_no_pull_request_number_cannot_retry() {
    // Reach ci-and-review without the pull request ever being opened, then
    // stop the review. The watch and the retry both need a number.
    let mut job = implementing(&["only"]);
    apply(&mut job, Event::Agent(AgentResult::CommitsReady(ready())));
    apply(&mut job, Event::ChecksPassed);
    apply(&mut job, Event::Published { number: 7 });
    apply(&mut job, start_review());
    apply(
        &mut job,
        Event::ReviewFailed {
            reason: "reviewer timed out".into(),
        },
    );
    assert!(job.can_retry());

    // Drop the number from the stored job, as if the pull request were never
    // opened, and the retry is rejected with the job left as it is.
    let mut value: serde_json::Value = serde_json::to_value(&job).unwrap();
    value["plan"][0]["number"] = serde_json::Value::Null;
    let mut job: Job = serde_json::from_value(value).unwrap();
    assert!(!job.can_retry());
    let before = job.clone();
    assert!(matches!(
        job.apply(Event::Retry),
        Err(Rejected::Unexpected { .. })
    ));
    assert_eq!(job, before);
}

#[test]
fn a_retry_is_rejected_at_every_other_user_ask() {
    let mut spec = Job::new("add a reboot button", Gates::default());
    apply(&mut spec, Event::Start);
    apply(&mut spec, Event::Agent(AgentResult::Clarified));
    apply(&mut spec, Event::Agent(AgentResult::Spec("spec".into())));
    assert_eq!(spec.turn(), &Turn::User(Ask::SpecApproval));

    let mut answers = Job::new("add a reboot button", Gates::default());
    apply(&mut answers, Event::Start);
    apply(
        &mut answers,
        Event::Agent(AgentResult::Questions(vec![question("Which board?")])),
    );

    for mut job in [spec, answers] {
        assert!(!job.can_retry());
        let before = job.clone();
        assert!(job.apply(Event::Retry).is_err());
        assert_eq!(job, before);
    }
}

#[test]
fn a_job_stored_in_review_stopped_without_the_stop_field_retries_as_a_reviewer_failure() {
    // Job #28 stopped before the `stop` field existed: its ReviewStopped ask
    // has only a reason. It loads as a reviewer failure and reruns the
    // reviewer.
    let mut job = watching(7);
    apply(&mut job, start_review());
    apply(
        &mut job,
        Event::ReviewFailed {
            reason: "You've hit your 5-hour limit".into(),
        },
    );
    let mut value: serde_json::Value = serde_json::to_value(&job).unwrap();
    value["turn"]["User"]["ReviewStopped"]
        .as_object_mut()
        .unwrap()
        .remove("stop")
        .unwrap();

    let mut old: Job = serde_json::from_value(value).unwrap();
    assert!(old.can_retry());
    apply(&mut old, Event::Retry);
    assert_eq!(old.stage(), &Stage::CiAndReview { pr: 0 });
    assert_runs(&old, Session::Review(0), Task::Review);
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
    apply(&mut review_capped, start_review());
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
            stop: ReviewStop::CapReached {
                feedback: "again".into(),
            },
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
    apply(&mut job, start_review());
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
    apply(&mut job, start_review());
    assert_runs(&job, Session::Review(0), Task::Review);

    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
}

#[test]
fn a_review_pass_survives_a_round_trip_through_json() {
    let mut job = watching(7);
    apply(&mut job, start_review());
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

/// A [`Event::StartReview`] on a head no earlier pass read, so the reviewer
/// runs.
fn start_review() -> Event {
    static HEADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = HEADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Event::StartReview {
        head: format!("head{n}"),
    }
}

const LIMITS: RetryLimits = RetryLimits { auto: 3, total: 7 };

fn renovate_job() -> Job {
    Job::renovate(
        42,
        "chore(deps): update serde to v1.0.2",
        "https://github.com/o/r/pull/42",
    )
}

fn observe(job: &mut Job, head: &str, ci: RenovateCi, limits: RetryLimits, auto_merge: bool) {
    apply(
        job,
        Event::RenovateObserved {
            head: head.into(),
            ci,
            limits,
            auto_merge,
        },
    );
}

fn failed(log: &str) -> RenovateCi {
    RenovateCi::Failed { log: log.into() }
}

fn passed() -> RenovateCi {
    RenovateCi::Passed {
        details: Some(Box::new(PrDiff {
            diff: "+serde = 1.0.2".into(),
            body: "Bumps serde".into(),
        })),
    }
}

fn pr_of(job: &Job) -> &factory_core::RenovatePr {
    job.renovate_pr().expect("a Renovate job")
}

/// Fails CI on `head` and runs the factory's rerun, as many times as the job
/// asks for a rerun with no agent.
fn rerun_ci(job: &mut Job, head: &str, limits: RetryLimits) {
    observe(job, head, failed("log"), limits, false);
    assert!(
        matches!(job.turn(), Turn::RerunCi { .. }),
        "expected a rerun, got {:?}",
        job.turn()
    );
    assert!(matches!(
        job.next_command(),
        Some(Command::RerunRenovate { number: 42, .. })
    ));
    apply(job, Event::CiRerun);
    assert_eq!(job.turn(), &Turn::Factory);
}

fn post_comment(job: &mut Job) -> String {
    let Some(Command::PostComment { number: 42, body }) = job.next_command() else {
        panic!("expected a comment, got {:?}", job.next_command());
    };
    apply(job, Event::Commented);
    body
}

fn waits(job: &Job, state: RenovateWait) {
    assert_eq!(job.stage(), &Stage::Renovate);
    assert!(
        matches!(job.turn(), Turn::User(Ask::Renovate { state: s, .. }) if *s == state),
        "expected {state:?}, got {:?}",
        job.turn()
    );
}

#[test]
fn a_renovate_job_starts_watching_its_pull_request() {
    let job = renovate_job();
    assert_eq!(job.stage(), &Stage::Renovate);
    assert_eq!(job.turn(), &Turn::Factory);
    assert_eq!(job.request(), "chore(deps): update serde to v1.0.2");
    assert_eq!(pr_of(&job).number, 42);
    assert_eq!(
        job.next_command(),
        Some(Command::WatchRenovate {
            number: 42,
            verdict_head: None,
            watch_only: false
        })
    );
}

#[test]
fn a_stored_job_without_a_kind_loads_as_a_request() {
    let mut stored =
        serde_json::to_value(Job::new("add a reboot button", Gates::default())).unwrap();
    stored.as_object_mut().unwrap().remove("kind").unwrap();
    let job: Job = serde_json::from_value(stored).unwrap();
    assert_eq!(job.kind(), &JobKind::Request);
    assert!(job.renovate_pr().is_none());
}

#[test]
fn a_renovate_job_survives_a_round_trip_through_json() {
    let mut job = renovate_job();
    observe(&mut job, "h1", failed("log"), LIMITS, false);
    let back: Job = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
    assert_eq!(job, back);
}

#[test]
fn pending_ci_and_no_checks_leave_the_job_running_and_never_merge() {
    for ci in [RenovateCi::NoChecks, RenovateCi::Pending] {
        let mut job = renovate_job();
        observe(&mut job, "h1", ci, LIMITS, true);
        assert_eq!(job.turn(), &Turn::Factory);
        assert!(matches!(
            job.next_command(),
            Some(Command::WatchRenovate { .. })
        ));
    }
}

#[test]
fn failing_ci_gets_the_automatic_reruns_and_then_the_agent() {
    let mut job = renovate_job();
    for n in 1..=3 {
        observe(&mut job, "h1", failed("log"), LIMITS, false);
        assert_eq!(
            job.turn(),
            &Turn::RerunCi {
                reason: format!("automatic rerun {n} of 3")
            }
        );
        apply(&mut job, Event::CiRerun);
    }
    assert_eq!((pr_of(&job).reruns, pr_of(&job).auto_reruns), (3, 3));
    observe(&mut job, "h1", failed("the log"), LIMITS, false);
    assert_runs(
        &job,
        Session::Renovate(1),
        Task::AnalyzeFailure {
            log: "the log".into(),
        },
    );
}

#[test]
fn one_automatic_rerun_sends_the_second_failure_to_the_agent() {
    let limits = RetryLimits { auto: 1, total: 7 };
    let mut job = renovate_job();
    rerun_ci(&mut job, "h1", limits);
    observe(&mut job, "h1", failed("log"), limits, false);
    assert!(matches!(
        job.turn(),
        Turn::Agent(Task::AnalyzeFailure { .. })
    ));
}

#[test]
fn zero_automatic_reruns_send_the_first_failure_to_the_agent() {
    let limits = RetryLimits { auto: 0, total: 2 };
    let mut job = renovate_job();
    observe(&mut job, "h1", failed("log"), limits, false);
    assert!(matches!(
        job.turn(),
        Turn::Agent(Task::AnalyzeFailure { .. })
    ));
}

#[test]
fn a_transient_verdict_reruns_and_counts_toward_the_total_only() {
    let mut job = renovate_job();
    for _ in 0..3 {
        rerun_ci(&mut job, "h1", LIMITS);
    }
    observe(&mut job, "h1", failed("log"), LIMITS, false);
    apply(
        &mut job,
        Event::Agent(AgentResult::FailureVerdict {
            transient: true,
            reason: "registry timeout".into(),
        }),
    );
    assert_eq!(
        job.turn(),
        &Turn::RerunCi {
            reason: "transient failure: registry timeout".into()
        }
    );
    apply(&mut job, Event::CiRerun);
    assert_eq!((pr_of(&job).reruns, pr_of(&job).auto_reruns), (4, 3));
    assert_eq!(job.turn(), &Turn::Factory);
}

#[test]
fn a_caused_by_update_verdict_posts_one_comment_and_stops_triage() {
    let mut job = renovate_job();
    let limits = RetryLimits { auto: 0, total: 7 };
    observe(&mut job, "h1", failed("log"), limits, false);
    apply(
        &mut job,
        Event::Agent(AgentResult::FailureVerdict {
            transient: false,
            reason: "the API was removed".into(),
        }),
    );
    let body = post_comment(&mut job);
    assert!(body.contains("the API was removed"));
    waits(&job, RenovateWait::Stopped);
    assert!(pr_of(&job).stopped.is_some());
    assert_eq!(
        pr_of(&job).comments,
        [("h1".to_owned(), CommentKind::CausedByUpdate)]
    );
    assert_eq!(
        job.next_command(),
        Some(Command::WatchRenovate {
            number: 42,
            verdict_head: None,
            watch_only: true
        })
    );

    // Watching the same failing head again reruns nothing and posts nothing.
    let before = job.clone();
    observe(&mut job, "h1", failed("log"), limits, false);
    assert_eq!(job, before);
}

#[test]
fn the_total_limit_posts_one_comment_naming_the_reruns_and_stops() {
    let mut job = renovate_job();
    for _ in 0..3 {
        rerun_ci(&mut job, "h1", LIMITS);
    }
    for _ in 3..7 {
        observe(&mut job, "h1", failed("log"), LIMITS, false);
        apply(
            &mut job,
            Event::Agent(AgentResult::FailureVerdict {
                transient: true,
                reason: "flaky".into(),
            }),
        );
        apply(&mut job, Event::CiRerun);
    }
    assert_eq!(pr_of(&job).reruns, 7);
    observe(&mut job, "h1", failed("log"), LIMITS, false);
    let body = post_comment(&mut job);
    assert!(body.contains("7 reruns"), "{body}");
    waits(&job, RenovateWait::Stopped);
    assert_eq!(pr_of(&job).reruns, 7);
}

#[test]
fn a_total_of_four_stops_after_four_reruns() {
    let limits = RetryLimits { auto: 3, total: 4 };
    let mut job = renovate_job();
    for _ in 0..3 {
        rerun_ci(&mut job, "h1", limits);
    }
    observe(&mut job, "h1", failed("log"), limits, false);
    apply(
        &mut job,
        Event::Agent(AgentResult::FailureVerdict {
            transient: true,
            reason: "flaky".into(),
        }),
    );
    apply(&mut job, Event::CiRerun);
    observe(&mut job, "h1", failed("log"), limits, false);
    assert!(post_comment(&mut job).contains("4 reruns"));
    waits(&job, RenovateWait::Stopped);
}

#[test]
fn a_lowered_total_stops_a_job_that_already_has_more_reruns() {
    let mut job = renovate_job();
    for _ in 0..3 {
        rerun_ci(&mut job, "h1", LIMITS);
    }
    for _ in 3..5 {
        observe(&mut job, "h1", failed("log"), LIMITS, false);
        apply(
            &mut job,
            Event::Agent(AgentResult::FailureVerdict {
                transient: true,
                reason: "flaky".into(),
            }),
        );
        apply(&mut job, Event::CiRerun);
    }
    assert_eq!(pr_of(&job).reruns, 5);
    let lowered = RetryLimits { auto: 3, total: 4 };
    observe(&mut job, "h1", failed("log"), lowered, false);
    assert!(matches!(
        job.next_command(),
        Some(Command::PostComment { .. })
    ));
    assert!(post_comment(&mut job).contains("5 reruns"));
    waits(&job, RenovateWait::Stopped);
}

#[test]
fn a_new_head_resets_the_counters_and_the_stop() {
    let limits = RetryLimits { auto: 1, total: 1 };
    let mut job = renovate_job();
    rerun_ci(&mut job, "h1", limits);
    observe(&mut job, "h1", failed("log"), limits, false);
    post_comment(&mut job);
    waits(&job, RenovateWait::Stopped);

    observe(&mut job, "h2", failed("log"), limits, false);
    assert_eq!((pr_of(&job).reruns, pr_of(&job).auto_reruns), (0, 0));
    assert_eq!(pr_of(&job).stopped, None);
    assert!(matches!(job.turn(), Turn::RerunCi { .. }));
}

#[test]
fn the_stopped_comment_is_posted_once_per_head() {
    let limits = RetryLimits { auto: 0, total: 1 };
    let mut job = renovate_job();
    observe(&mut job, "h1", failed("log"), limits, false);
    apply(
        &mut job,
        Event::Agent(AgentResult::FailureVerdict {
            transient: true,
            reason: "flaky".into(),
        }),
    );
    apply(&mut job, Event::CiRerun);
    observe(&mut job, "h1", failed("log"), limits, false);
    post_comment(&mut job);

    // Retry resets the counters on the same head. A second stop posts nothing.
    apply(&mut job, Event::Retry);
    apply(&mut job, Event::CiRerun);
    assert_eq!((pr_of(&job).reruns, pr_of(&job).auto_reruns), (0, 0));
    let limits = RetryLimits { auto: 0, total: 0 };
    observe(&mut job, "h1", failed("log"), limits, false);
    waits(&job, RenovateWait::Stopped);
    assert_eq!(pr_of(&job).comments.len(), 1);

    // Another head gets its own comment.
    observe(&mut job, "h2", failed("log"), limits, false);
    assert!(matches!(
        job.next_command(),
        Some(Command::PostComment { .. })
    ));
}

#[test]
fn retry_resumes_a_stopped_job_with_both_counters_at_zero() {
    let limits = RetryLimits { auto: 1, total: 1 };
    let mut job = renovate_job();
    rerun_ci(&mut job, "h1", limits);
    observe(&mut job, "h1", failed("log"), limits, false);
    post_comment(&mut job);
    assert!(job.can_retry());

    apply(&mut job, Event::Retry);
    assert!(matches!(
        job.next_command(),
        Some(Command::RerunRenovate { number: 42, .. })
    ));
    apply(&mut job, Event::CiRerun);
    assert_eq!((pr_of(&job).reruns, pr_of(&job).auto_reruns), (0, 0));
    assert_eq!(pr_of(&job).stopped, None);
    assert_eq!(job.turn(), &Turn::Factory);

    // A job that does not wait stopped cannot retry.
    assert!(!job.can_retry());
    assert!(job.clone().apply(Event::Retry).is_err());
}

#[test]
fn passing_ci_asks_the_agent_to_check_the_diff_once_per_head() {
    let mut job = renovate_job();
    observe(&mut job, "h1", passed(), LIMITS, true);
    assert_runs(
        &job,
        Session::Renovate(1),
        Task::CheckDiff {
            diff: "+serde = 1.0.2".into(),
            body: "Bumps serde".into(),
        },
    );
    apply(
        &mut job,
        Event::Agent(AgentResult::DiffVerdict(Box::new(DiffVerdict {
            safe: true,
            reason: "only a version changes".into(),
            summary: "serde 1.0.1 to 1.0.2".into(),
            from: Some("v1.0.1".into()),
            to: Some("v1.0.2".into()),
        }))),
    );
    let verdict = pr_of(&job).verdict.clone().unwrap();
    assert_eq!(verdict.head, "h1");
    assert_eq!(verdict.from.as_deref(), Some("v1.0.1"));
    assert_eq!(
        job.next_command(),
        Some(Command::WatchRenovate {
            number: 42,
            verdict_head: Some("h1".into()),
            watch_only: false
        })
    );

    // A new head gets a new check.
    observe(&mut job, "h2", passed(), LIMITS, true);
    assert!(matches!(job.turn(), Turn::Agent(Task::CheckDiff { .. })));
    assert_eq!(pr_of(&job).verdict, None);
}

fn with_safe_verdict(auto_merge: bool) -> Job {
    let mut job = renovate_job();
    observe(&mut job, "h1", passed(), LIMITS, auto_merge);
    apply(
        &mut job,
        Event::Agent(AgentResult::DiffVerdict(Box::new(DiffVerdict {
            safe: true,
            reason: "only a version changes".into(),
            summary: "bump".into(),
            from: None,
            to: None,
        }))),
    );
    job
}

#[test]
fn a_safe_verdict_with_auto_merge_on_approves_and_merges_that_head() {
    let mut job = with_safe_verdict(true);
    observe(
        &mut job,
        "h1",
        RenovateCi::Passed { details: None },
        LIMITS,
        true,
    );
    assert_eq!(
        job.next_command(),
        Some(Command::MergeRenovate {
            number: 42,
            head: "h1".into()
        })
    );
    apply(&mut job, Event::Merged);
    assert_eq!(job.stage(), &Stage::Merged);
    assert_eq!(job.turn(), &Turn::Done);
    assert!(job.is_finished());
}

#[test]
fn a_safe_verdict_with_auto_merge_off_waits_as_ready_to_merge() {
    let mut job = with_safe_verdict(false);
    observe(
        &mut job,
        "h1",
        RenovateCi::Passed { details: None },
        LIMITS,
        false,
    );
    waits(&job, RenovateWait::Ready);
    assert!(!job.can_retry());
    assert!(matches!(
        job.next_command(),
        Some(Command::WatchRenovate {
            watch_only: false,
            ..
        })
    ));

    // Turning auto-merge on merges the ready job on the next watch.
    observe(
        &mut job,
        "h1",
        RenovateCi::Passed { details: None },
        LIMITS,
        true,
    );
    assert!(matches!(
        job.next_command(),
        Some(Command::MergeRenovate { .. })
    ));
}

#[test]
fn an_unsafe_verdict_posts_one_comment_and_never_merges() {
    let mut job = renovate_job();
    observe(&mut job, "h1", passed(), LIMITS, true);
    apply(
        &mut job,
        Event::Agent(AgentResult::DiffVerdict(Box::new(DiffVerdict {
            safe: false,
            reason: "it edits a workflow".into(),
            summary: "bump".into(),
            from: None,
            to: None,
        }))),
    );
    assert!(post_comment(&mut job).contains("it edits a workflow"));
    waits(&job, RenovateWait::Unsafe);
    assert!(!job.can_retry());
    assert!(matches!(
        job.next_command(),
        Some(Command::WatchRenovate {
            watch_only: true,
            ..
        })
    ));

    let before = job.clone();
    observe(
        &mut job,
        "h1",
        RenovateCi::Passed { details: None },
        LIMITS,
        true,
    );
    assert_eq!(job, before);
    assert_eq!(
        pr_of(&job).comments,
        [("h1".to_owned(), CommentKind::Unsafe)]
    );
}

#[test]
fn a_renovate_job_is_merged_or_cancelled_from_any_wait() {
    let mut merged = renovate_job();
    apply(&mut merged, Event::Merged);
    assert_eq!(merged.stage(), &Stage::Merged);

    let mut closed = renovate_job();
    observe(
        &mut closed,
        "h1",
        failed("log"),
        RetryLimits { auto: 0, total: 1 },
        false,
    );
    apply(&mut closed, Event::ClosedUnmerged);
    assert_eq!(closed.stage(), &Stage::Cancelled);
    assert_eq!(closed.turn(), &Turn::Done);
    assert!(closed.is_finished());
}

#[test]
fn a_failed_renovate_agent_run_retries_the_same_task() {
    let mut job = renovate_job();
    observe(
        &mut job,
        "h1",
        failed("log"),
        RetryLimits { auto: 0, total: 7 },
        false,
    );
    let task = Task::AnalyzeFailure { log: "log".into() };
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "the agent run ended with error_max_turns".into(),
        },
    );
    assert!(job.can_retry());
    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::Renovate);
    assert_eq!(job.turn(), &Turn::Agent(task));
    assert_eq!(pr_of(&job).head.as_deref(), Some("h1"));
}

#[test]
fn each_renovate_agent_run_gets_its_own_session_and_a_retry_keeps_it() {
    let limits = RetryLimits { auto: 0, total: 7 };
    let mut job = renovate_job();
    observe(&mut job, "h1", failed("log"), limits, false);
    assert_runs(
        &job,
        Session::Renovate(1),
        Task::AnalyzeFailure { log: "log".into() },
    );
    apply(
        &mut job,
        Event::Agent(AgentResult::FailureVerdict {
            transient: true,
            reason: "network".into(),
        }),
    );
    apply(&mut job, Event::CiRerun);
    observe(&mut job, "h1", passed(), limits, false);
    let check = Task::CheckDiff {
        diff: "+serde = 1.0.2".into(),
        body: "Bumps serde".into(),
    };
    assert_runs(&job, Session::Renovate(2), check.clone());
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "timed out".into(),
        },
    );
    apply(&mut job, Event::Retry);
    assert_runs(&job, Session::Renovate(2), check);
}

#[test]
fn a_renovate_job_rejects_events_that_do_not_fit() {
    let mut job = renovate_job();
    assert!(job.apply(Event::CiRerun).is_err());
    assert!(job.apply(Event::Commented).is_err());
    assert!(job.apply(Event::Start).is_err());
    observe(
        &mut job,
        "h1",
        failed("log"),
        RetryLimits { auto: 0, total: 0 },
        false,
    );
    // The comment is posted before the next observation.
    assert!(
        job.apply(Event::RenovateObserved {
            head: "h1".into(),
            ci: RenovateCi::Pending,
            limits: LIMITS,
            auto_merge: false,
        })
        .is_err()
    );
}

fn proposal() -> Job {
    Job::proposal(
        "Add the new build flag",
        Gates {
            spec: true,
            plan: true,
            publish: false,
        },
        7,
        42,
    )
}

#[test]
fn a_proposal_waits_for_the_user_and_runs_nothing() {
    let job = proposal();
    assert_eq!(job.stage(), &Stage::Proposed);
    assert_eq!(job.turn(), &Turn::User(Ask::Proposal));
    assert_eq!(job.next_command(), None);
    assert_eq!(job.gap_source(), Some((7, 42)));
    assert!(!job.is_finished());
    assert!(!job.can_retry());
}

#[test]
fn approving_a_proposal_queues_it_with_its_gates() {
    let mut job = proposal();
    apply(&mut job, Event::Approve);
    assert_eq!(job.stage(), &Stage::Requested);
    assert_eq!(job.turn(), &Turn::Factory);
    assert!(job.gates().spec && job.gates().plan);

    apply(&mut job, Event::Start);
    assert_eq!(job.stage(), &Stage::Clarifying);
    assert_eq!(job.gap_source(), Some((7, 42)));
}

#[test]
fn dismissing_a_proposal_ends_it_dismissed() {
    let mut job = proposal();
    apply(&mut job, Event::Dismiss);
    assert_eq!(job.stage(), &Stage::Dismissed);
    assert_eq!(job.turn(), &Turn::Done);
    assert!(job.is_dismissed());
    assert!(job.is_finished());
    assert!(!job.can_retry());
    assert_eq!(job.next_command(), None);

    let before = job.clone();
    for event in [Event::Dismiss, Event::Approve, Event::Retry] {
        assert!(matches!(job.apply(event), Err(Rejected::Unexpected { .. })));
    }
    assert_eq!(job, before);
}

#[test]
fn a_proposal_rejects_edit_and_retry() {
    let mut job = proposal();
    let before = job.clone();
    for event in [
        Event::Edit {
            request: "other".into(),
        },
        Event::Retry,
    ] {
        assert!(matches!(job.apply(event), Err(Rejected::Unexpected { .. })));
    }
    assert_eq!(job, before);
}

fn context() -> factory_core::PackagingContext {
    factory_core::PackagingContext {
        kind: "snap".into(),
        upstream_repo: "up/app".into(),
        pr_title: "Update app".into(),
        pr_body: "Bumps app".into(),
        pr_diff: "+app 2".into(),
        manifests: Vec::new(),
        from: None,
        to: None,
        upstream: Vec::new(),
        existing: Vec::new(),
    }
}

fn gap() -> factory_core::Gap {
    factory_core::Gap {
        title: "Add the flag".into(),
        description: "d".into(),
        evidence: "e".into(),
        current: "c".into(),
        suggestion: "s".into(),
    }
}

fn gathering() -> Job {
    let mut job = renovate_job();
    apply(
        &mut job,
        Event::RenovateMerged {
            packaging: true,
            manifest_changed: true,
        },
    );
    job
}

#[test]
fn a_merge_without_a_changed_manifest_or_a_packaging_project_ends_the_job() {
    for (packaging, manifest_changed) in [(false, false), (false, true), (true, false)] {
        let mut job = renovate_job();
        apply(
            &mut job,
            Event::RenovateMerged {
                packaging,
                manifest_changed,
            },
        );
        assert_eq!(job.stage(), &Stage::Merged);
        assert!(job.is_finished());
    }
}

#[test]
fn a_merged_manifest_gathers_then_analyses_then_proposes_then_ends() {
    let mut job = gathering();
    assert_eq!(job.stage(), &Stage::Packaging);
    assert_eq!(
        job.next_command(),
        Some(Command::GatherPackaging { number: 42 })
    );

    apply(&mut job, Event::PackagingGathered(Box::new(context())));
    assert!(matches!(job.turn(), Turn::Agent(Task::FindGaps { .. })));
    assert!(matches!(
        job.next_command(),
        Some(Command::RunAgent {
            task: Task::FindGaps { .. },
            ..
        })
    ));

    apply(&mut job, Event::Agent(AgentResult::Gaps(vec![gap()])));
    assert_eq!(
        job.next_command(),
        Some(Command::ProposeGaps { gaps: vec![gap()] })
    );

    apply(&mut job, Event::GapsProposed { jobs: vec![5, 6] });
    assert_eq!(job.stage(), &Stage::Merged);
    assert!(job.is_finished());
    assert_eq!(pr_of(&job).gaps, Some(vec![5, 6]));
}

#[test]
fn no_gaps_is_a_valid_result() {
    let mut job = gathering();
    apply(&mut job, Event::PackagingGathered(Box::new(context())));
    apply(&mut job, Event::Agent(AgentResult::Gaps(Vec::new())));
    apply(&mut job, Event::GapsProposed { jobs: Vec::new() });
    assert_eq!(job.stage(), &Stage::Merged);
    assert_eq!(pr_of(&job).gaps, Some(Vec::new()));
}

#[test]
fn a_failed_analysis_run_can_be_retried() {
    let mut job = gathering();
    apply(&mut job, Event::PackagingGathered(Box::new(context())));
    apply(
        &mut job,
        Event::AgentFailed {
            reason: "timed out".into(),
        },
    );
    assert!(job.can_retry());
    apply(&mut job, Event::Retry);
    assert_eq!(job.stage(), &Stage::Packaging);
}
