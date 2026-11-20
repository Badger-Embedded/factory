use std::fmt::Write;

use factory_core::{Job, PlannedPr, Task};

use crate::outcome::Expect;

/// The prompt for one agent run.
#[must_use]
pub fn prompt(task: &Task, expect: Expect) -> String {
    let mut text = match task {
        Task::Clarify { request } => format!(
            "This is a feature request for the repository in the current directory:\n\n\
             {request}\n\n\
             Read the repository until you know what the request touches. Do not change \
             any files. If the answer to a question would change what you build and the \
             repository does not answer it, ask the requester."
        ),
        Task::WriteSpec => "Write the spec for the request: the goal, what is in scope, what \
             is out of scope, and acceptance criteria a reviewer can check. Do not change any \
             files."
            .to_owned(),
        Task::WritePlan => "The requester approved the spec. Split the work into pull \
             requests that follow the repository's CONTRIBUTING.md: one change per pull \
             request, and a large refactor first in its own pull request. Give each pull \
             request a title in the repository's title format and a summary of what it \
             changes. Do not change any files."
            .to_owned(),
        Task::Implement { pr } => format!(
            "The requester approved the plan. Implement this pull request:\n\n\
             {}\n\n{}\n\n\
             Follow the repository's CLAUDE.md and CONTRIBUTING.md, tests included. Commit \
             your work on the current branch. Do not push or open a pull request. The \
             factory does that with the title and description you return, so write them \
             the way CONTRIBUTING.md asks.",
            pr.title, pr.summary
        ),
        Task::Answer { questions, answers } => {
            let mut text = "The requester answered your questions:\n".to_owned();
            for (question, answer) in questions.iter().zip(answers) {
                let _ = write!(text, "\nQ: {}\nA: {answer}\n", question.text);
            }
            text.push_str("\nContinue the task.");
            text
        }
        Task::Revise { feedback } => {
            format!("The requester asked for changes:\n\n{feedback}\n\nRevise your work.")
        }
        Task::FixChecks { report } => format!(
            "The project's checks failed on your commits:\n\n{report}\n\n\
             Fix the failures and commit."
        ),
        Task::FixCi { log } => {
            format!(
                "CI failed on the pull request:\n\n{log}\n\n\
                 If the failure comes from the code, fix it and commit. If the log shows a \
                 failure unrelated to this branch, such as a flaky test or a network error, \
                 and there is nothing to commit, end with outcome \"rerun_ci\" and a reason \
                 that cites the log. The factory then reruns the failed CI runs."
            )
        }
        Task::Rebase { base } => format!(
            "The pull request no longer merges cleanly into {base}. The factory fetched \
             the newest origin/{base} into this checkout. Rebase the branch onto \
             origin/{base} and resolve the conflicts so that both your change and the \
             changes on {base} keep working. Run the tests. Do not push. Update the pull \
             request title and description if the rebase changed what they describe."
        ),
        Task::AddressReview { comments } => format!(
            "The reviewer left comments on the pull request:\n\n{comments}\n\n\
             Address them and commit."
        ),
        Task::Review => return review_prompt(),
    };
    let outcomes = expect.outcomes();
    let (questions, done) = (outcomes[0], outcomes[1]);
    let _ = write!(
        text,
        "\n\nEnd the run with the structured output. Use outcome \"{done}\" when the task is \
         done. Use outcome \"{questions}\" only when you need an answer from the requester to \
         continue, and list the questions."
    );
    text
}

/// The prompt for an LLM review run. The worker has no GitHub token, so the
/// reviewer reads the branch's diff from the checkout and does not push. It
/// resumes across passes, so a later pass reviews only the new commits.
fn review_prompt() -> String {
    "Review the commits this branch adds on top of its base. Read the diff with git, \
     against the base branch the checkout is on. Check the change for correctness and \
     for the repository's own rules in CLAUDE.md and CONTRIBUTING.md. Do not change any \
     files and do not push.\n\n\
     End with outcome \"approve\" when the change is ready to merge, or \"changes\" with \
     feedback the author can act on when it is not."
        .to_owned()
}

/// The prompt for one agent run of `job`. A job with fresh sessions starts
/// its spec, plan and implement runs in a new session, so those prompts
/// carry what the session has not seen. Every other run uses [`prompt`].
#[must_use]
pub fn job_prompt(job: &Job, task: &Task, expect: Expect) -> String {
    let body = prompt(task, expect);
    if !job.fresh_sessions() {
        return body;
    }
    let mut context = String::new();
    match task {
        Task::WriteSpec => request_section(&mut context, job),
        Task::WritePlan => {
            request_section(&mut context, job);
            spec_section(&mut context, job);
        }
        Task::Implement { pr } => {
            request_section(&mut context, job);
            spec_section(&mut context, job);
            plan_section(&mut context, job, pr);
        }
        _ => return body,
    }
    format!("{context}{body}")
}

fn request_section(text: &mut String, job: &Job) {
    let _ = write!(
        text,
        "This is a feature request for the repository in the current directory:\n\n{}\n\n",
        job.request()
    );
    if !job.answers().is_empty() {
        text.push_str("The requester answered these questions about it:\n");
        for answered in job.answers() {
            let _ = write!(text, "\nQ: {}\nA: {}\n", answered.question, answered.answer);
        }
        text.push('\n');
    }
}

fn spec_section(text: &mut String, job: &Job) {
    let _ = write!(
        text,
        "The approved spec:\n\n{}\n\n",
        job.spec().unwrap_or_default()
    );
}

fn plan_section(text: &mut String, job: &Job, current: &PlannedPr) {
    text.push_str("The approved plan splits the work into these pull requests:\n");
    for (index, planned) in job.plan().iter().enumerate() {
        let _ = write!(text, "\n{}. {}", index + 1, planned.title);
        if planned.title == current.title {
            text.push_str(" (the one to implement now)");
        }
    }
    text.push_str("\n\n");
}
