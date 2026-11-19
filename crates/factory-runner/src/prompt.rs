use std::fmt::Write;

use factory_core::Task;

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
             factory does that.",
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
            format!("CI failed on the pull request:\n\n{log}\n\nFix the failure and commit.")
        }
        Task::AddressReview { comments } => format!(
            "The reviewer left comments on the pull request:\n\n{comments}\n\n\
             Address them and commit."
        ),
    };
    let [questions, done] = expect.outcomes();
    let _ = write!(
        text,
        "\n\nEnd the run with the structured output. Use outcome \"{done}\" when the task is \
         done. Use outcome \"{questions}\" only when you need an answer from the requester to \
         continue, and list the questions."
    );
    text
}
