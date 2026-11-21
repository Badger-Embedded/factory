use std::fmt::Write;

use factory_core::{Agent, Answered, Job, PlannedPr, Task, Verdict};

use crate::outcome::Expect;

/// The prompt for one agent run. `agent` picks the repository instructions file
/// the prompt names, `CLAUDE.md` for Claude and `AGENTS.md` for Codex, and the
/// review method a review run follows.
#[must_use]
pub fn prompt(task: &Task, expect: Expect, agent: Agent) -> String {
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
             changes. Each pull request is implemented in a new session that has not seen \
             the repository, so give each one notes with what you found that it needs: the \
             files and functions to change, the tests to extend, and the docs that apply. \
             Do not change any files."
            .to_owned(),
        Task::Implement { pr } => {
            let mut text = format!(
                "The requester approved the plan. Implement this pull request:\n\n{}\n\n{}\n\n",
                pr.title, pr.summary
            );
            if let Some(notes) = &pr.notes {
                let _ = write!(
                    text,
                    "The planning run left these notes. It wrote them before the earlier \
                     pull requests in the plan were implemented, so check them against the \
                     code:\n\n{notes}\n\n"
                );
            }
            let _ = write!(
                text,
                "Follow the repository's {} and CONTRIBUTING.md, tests included. Commit \
                 your work on the current branch. Do not push or open a pull request. The \
                 factory does that with the title and description you return, so write them \
                 the way CONTRIBUTING.md asks.",
                agent.rules_doc()
            );
            text
        }
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
        Task::Review => return review_prompt(&ReviewContext::default(), agent),
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

/// What a review pass reads besides the diff.
#[derive(Default)]
struct ReviewContext<'a> {
    /// The request, the requester's answers, the approved spec and the plan,
    /// for the first pass on a pull request. Empty on a later pass, whose
    /// resumed session already holds them.
    decisions: String,
    /// The answers the requester gave since the last pass, on a later pass.
    new_answers: &'a [Answered],
    /// The feedback the last pass asked for, when it asked for changes.
    previous: Option<&'a str>,
}

impl<'a> ReviewContext<'a> {
    fn for_job(job: &'a Job) -> Self {
        let Some(pr) = job.stage().pr() else {
            return ReviewContext::default();
        };
        let Some(last) = job.reviews_of(pr).last() else {
            let mut decisions = String::new();
            request_section(&mut decisions, job);
            if job.spec().is_some() {
                spec_section(&mut decisions, job);
            }
            if let Some(planned) = job.plan().get(pr) {
                plan_section(&mut decisions, job, planned);
                let _ = write!(
                    decisions,
                    "This branch implements:\n\n{}\n\n{}\n\n",
                    planned.title, planned.summary
                );
            }
            return ReviewContext {
                decisions,
                ..ReviewContext::default()
            };
        };
        ReviewContext {
            decisions: String::new(),
            new_answers: job.answers().get(last.answers_seen..).unwrap_or_default(),
            previous: (last.verdict == Verdict::Changes)
                .then_some(last.feedback.as_deref())
                .flatten(),
        }
    }
}

/// The prompt for an LLM review run. The worker has no GitHub token, so the
/// reviewer reads the branch's diff from the checkout and does not push. The
/// session resumes across passes, so the first pass on a pull request gets the
/// request, answers, spec and plan, and a later pass gets only the answers
/// given since, with the feedback it asked for when it asked for changes.
fn review_prompt(context: &ReviewContext, agent: Agent) -> String {
    let mut text = context.decisions.clone();
    let _ = write!(
        text,
        "Review the commits this branch adds on top of its base. Read the diff with git, \
         against the base branch the checkout is on. Check the change for correctness and \
         for the repository's own rules in {} and CONTRIBUTING.md. Do not change any \
         files and do not push.\n\n",
        agent.rules_doc()
    );
    if !context.decisions.is_empty() {
        text.push_str(DECISIONS_RULE);
    }
    // Claude loads the factory's review skill through `--plugin-dir`; Codex has
    // no such flag and finds skills by name, so its prompt names the method's
    // path to keep it from a reviewed repository's own skill called `review`.
    match agent {
        Agent::Claude => text.push_str(
            "The factory's review skill describes the method. The repository you are reviewing \
             may carry its own review, lint or project skills; use them, since they know rules \
             this prompt does not.\n\n",
        ),
        Agent::Codex => text.push_str(
            "Follow the method in /factory-plugin/skills/review/SKILL.md, not any skill named \
             review in the repository you are reviewing. That repository may carry its own lint \
             or project skills; use them, since they know rules this prompt does not.\n\n",
        ),
    }
    if !context.new_answers.is_empty() {
        text.push_str("Since your last review, the requester answered these questions:\n");
        for answered in context.new_answers {
            let _ = write!(text, "\nQ: {}\nA: {}\n", answered.question, answered.answer);
        }
        text.push('\n');
        text.push_str(DECISIONS_RULE);
    }
    if let Some(feedback) = context.previous {
        let _ = write!(
            text,
            "You reviewed an earlier version of this branch and asked for these changes:\n\n\
             {feedback}\n\n\
             The author has pushed commits since. Review only the commits added since your \
             last review, and check that each point above is handled.\n\n"
        );
    }
    text.push_str(
        "End with outcome \"approve\" when the change is ready to merge, or \"changes\" with \
         feedback the author can act on when it is not.",
    );
    text
}

/// The requester's answers and the approved spec settle what the branch does.
/// A reviewer that asks to change one repeats a question the requester already
/// answered, and the author cannot act on it.
const DECISIONS_RULE: &str = "The requester's answers and the approved spec are decisions. Do not \
     ask for a change that contradicts one of them. If you think a decision is wrong, say so \
     in your feedback, and do not let it alone decide the verdict.\n\n";

/// The prompt for one agent run of `job`. A job with fresh sessions starts
/// its spec, plan and implement runs in a new session, so those prompts
/// carry what the session has not seen. Every other run uses [`prompt`].
#[must_use]
pub fn job_prompt(job: &Job, task: &Task, expect: Expect, agent: Agent) -> String {
    if let Task::Review = task {
        return review_prompt(&ReviewContext::for_job(job), agent);
    }
    let body = prompt(task, expect, agent);
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
