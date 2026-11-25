use std::fmt::Write;

use factory_core::{Agent, Answered, FileText, Job, PackagingContext, PlannedPr, Task, Verdict};

use crate::agent::{PromptStyle, backend_for};
use crate::outcome::Expect;

fn style(agent: Agent) -> PromptStyle {
    agent
        .cli()
        .and_then(backend_for)
        .expect("the run's CLI has a worker backend")
        .prompt_style()
}

/// The prompt for one agent run. `agent` picks the per-harness wording from its
/// backend: the repository instructions file the prompt names and the sentence
/// a review run follows to find its method.
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
                style(agent).rules_doc
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
        Task::AnalyzeFailure { log } => {
            return failure_prompt(log, agent);
        }
        Task::CheckDiff { diff, body } => return diff_prompt(diff, body, agent),
        Task::FindGaps { context } => return gaps_prompt(context, agent),
    };
    let outcomes = expect.outcomes();
    let (questions, done) = (outcomes[0], outcomes[1]);
    let _ = write!(
        text,
        "\n\nEnd the run with the structured output. Use outcome \"{done}\" when the task is \
         done. Use outcome \"{questions}\" only when you need an answer from the requester to \
         continue, and list the questions."
    );
    end_prompt(&mut text, agent, expect);
    text
}

fn end_prompt(text: &mut String, agent: Agent, expect: Expect) {
    text.push_str("\n\n");
    text.push_str(NIX_PARAGRAPH);
    append_schema(text, agent, expect);
}

const NIX_PARAGRAPH: &str = "Nix is installed in this environment, with flakes enabled. When a tool or \
     binary the work needs is missing, try it from nixpkgs before you conclude it is unavailable, \
     for example `nix shell nixpkgs#<package> -c <command>`. The repository's own instructions \
     still decide how to build and test: if they name a dev shell or a command, use that. This is \
     for tools that neither the repository's setup nor this environment provides.";

/// Appends the stage's JSON schema to the prompt when the agent's backend has
/// no output-schema flag and asks for it in the prompt. The backend owns the
/// wording, so this names no harness. A backend that passes the schema to its
/// CLI leaves it empty and nothing is appended.
fn append_schema(text: &mut String, agent: Agent, expect: Expect) {
    if let Some(instruction) = style(agent).schema_in_prompt {
        let schema = serde_json::to_string_pretty(&expect.schema())
            .unwrap_or_else(|_| expect.schema().to_string());
        let _ = write!(text, "\n\n{instruction}\n\n{schema}");
    }
}

fn failure_prompt(log: &str, agent: Agent) -> String {
    let mut text = format!(
        "CI failed on a Renovate pull request that updates a dependency, and reruns of the \
         failed runs did not fix it. This is the log:\n\n{log}\n\n\
         Decide whether the failure is transient or caused by the update. A transient \
         failure has nothing to do with the update: a flaky test, a network error, a \
         runner that ran out of resources or an unavailable service. A failure caused by \
         the update is one the new version explains, such as a changed API, a failing \
         test of the updated package or a build that no longer resolves. Read the \
         repository where the log leaves the question open. Do not change any files and do \
         not push.\n\n\
         End with outcome \"transient\" or \"caused_by_update\", and a reason that cites \
         the lines of the log that show it."
    );
    end_prompt(&mut text, agent, Expect::FailureAnalysis);
    text
}

/// The prompt for the diff check of a Renovate pull request. The worker has
/// no GitHub token, so the prompt carries the diff.
fn diff_prompt(diff: &str, body: &str, agent: Agent) -> String {
    let mut text = format!(
        "This is a Renovate pull request, and all its checks passed. Its description:\n\n\
         {body}\n\n\
         Its diff:\n\n{diff}\n\n\
         Decide whether the diff is safe to merge without a person reading it. The diff is \
         safe when it changes only dependency or packaging version files. Any other change \
         is unsafe, such as source code, CI workflows, scripts or configuration that does \
         more than pin a version. Do not change any files and do not push.\n\n\
         End with outcome \"safe\" or \"unsafe\", a reason that names the files that decide \
         it, and a short summary of the update. When the diff shows the upstream versions \
         or refs the update moves between, give them as \"from\" and \"to\"."
    );
    end_prompt(&mut text, agent, Expect::DiffCheck);
    text
}

fn gap_rules(kind: &str) -> &'static str {
    match kind {
        "snap" => {
            "For a snap, look for:\n\
             - new cmake `option()` flags, especially optional features that are off by \
             default, that `cmake-parameters` in snapcraft.yaml does not set;\n\
             - optional backends, such as GPU acceleration or hardware-specific libraries, \
             that `stage-packages` does not include;\n\
             - new interfaces the snap needs for new hardware access or system features;\n\
             - new environment variables the upstream binary now respects;\n\
             - new hooks that would improve the user experience."
        }
        "deb" => {
            "For a deb, look for:\n\
             - new Build-Depends that optional features need;\n\
             - new cmake or meson flags in `debian/rules` that enable useful features;\n\
             - new runtime Depends for optional functionality;\n\
             - changes to `debian/patches` that may need updating."
        }
        _ => {
            "Look broadly for new build flags, runtime dependencies, optional feature flags \
             and config files."
        }
    }
}

fn file_section(text: &mut String, title: &str, files: &[FileText], missing: &str) {
    let _ = write!(text, "{title}\n\n");
    if files.is_empty() {
        text.push_str("None are configured.\n\n");
    }
    for file in files {
        match &file.text {
            Some(content) => {
                let _ = write!(text, "File {}:\n\n{content}\n\n", file.path);
            }
            None => {
                let _ = write!(text, "File {}: {missing}\n\n", file.path);
            }
        }
    }
}

/// The prompt for the packaging analysis. The worker has no GitHub token, so
/// the prompt carries the manifest, the pull request and the upstream patches.
fn gaps_prompt(context: &PackagingContext, agent: Agent) -> String {
    let mut text = format!(
        "A Renovate pull request that updates the upstream project {} merged into this \
         {} packaging repository. Find the packaging gaps: things the update introduced \
         upstream that the packaging does not reflect.\n\n",
        context.upstream_repo, context.kind
    );
    let _ = write!(
        text,
        "The pull request is titled {:?}. Its description:\n\n{}\n\nIts diff:\n\n{}\n\n",
        context.pr_title, context.pr_body, context.pr_diff
    );
    file_section(
        &mut text,
        "The packaging manifest files as they are on the base branch after the merge:",
        &context.manifests,
        "it does not exist.",
    );
    match (&context.from, &context.to) {
        (Some(from), Some(to)) => {
            let _ = write!(
                text,
                "The update moves {} from {from} to {to}. These are the upstream changes to \
                 its build configuration files between the two:\n\n",
                context.upstream_repo
            );
            if context.upstream.is_empty() {
                text.push_str("No build configuration files are configured. Use the manifest and the release notes in the description.\n\n");
            }
            for file in &context.upstream {
                match &file.text {
                    Some(patch) => {
                        let _ = write!(text, "Patch of {}:\n\n{patch}\n\n", file.path);
                    }
                    None => {
                        let _ = write!(text, "{} did not change.\n\n", file.path);
                    }
                }
            }
        }
        _ => text.push_str(
            "The upstream changes are unavailable, because the factory never found the \
             versions the update moves between. Use the manifest and the release notes in \
             the description.\n\n",
        ),
    }
    if !context.existing.is_empty() {
        text.push_str(
            "The project already has these gap jobs. Leave out a gap that one of them \
             covers, whether it waits for approval, is in progress or was dismissed:\n",
        );
        for existing in &context.existing {
            let state = if existing.dismissed {
                "dismissed"
            } else {
                "proposed or in progress"
            };
            let _ = write!(text, "\n--- {state}\n{}\n", existing.request);
        }
        text.push('\n');
    }
    let _ = write!(
        text,
        "{}\n\n\
         Do not report internal refactors, test-only changes, removals of deprecated \
         features, API-only changes with no user-facing effect, or changes the manifest \
         already reflects. Report each distinct gap once, with the upstream evidence, what \
         the packaging has now and the change you suggest. Do not change any files and do \
         not push.\n\n\
         End with outcome \"gaps\" and the list of gaps. The list is empty when you found \
         none.",
        gap_rules(&context.kind)
    );
    end_prompt(&mut text, agent, Expect::PackagingGaps);
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
    let style = style(agent);
    let mut text = context.decisions.clone();
    let _ = write!(
        text,
        "Review the commits this branch adds on top of its base. Read the diff with git, \
         against the base branch the checkout is on. Check the change for correctness and \
         for the repository's own rules in {} and CONTRIBUTING.md. Do not change any \
         files and do not push.\n\n",
        style.rules_doc
    );
    if !context.decisions.is_empty() {
        text.push_str(DECISIONS_RULE);
    }
    text.push_str(style.review_method);
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
    end_prompt(&mut text, agent, Expect::Review);
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
///
/// Every prompt carries the nix paragraph once, before the schema instruction.
///
/// `mcp_servers` is the run's active MCP server names, in config order. When it
/// is non-empty the prompt ends with a paragraph that names them and tells the
/// agent to load their tools first, so a later step reaches a tool the harness
/// would otherwise keep behind a search. It also says that a gateway server's
/// listed tools only search for and run other tools. It is empty for a run with
/// no server.
#[must_use]
pub fn job_prompt(
    job: &Job,
    task: &Task,
    expect: Expect,
    agent: Agent,
    mcp_servers: &[String],
) -> String {
    let mut text = job_body(job, task, expect, agent);
    append_mcp(&mut text, mcp_servers);
    text
}

fn job_body(job: &Job, task: &Task, expect: Expect, agent: Agent) -> String {
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

fn append_mcp(text: &mut String, servers: &[String]) {
    if servers.is_empty() {
        return;
    }
    let names = servers
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = write!(
        text,
        "\n\nThis run has these MCP servers: {names}. Before you start, load the tools each one \
         exposes and read what they do, so you know them when a later step needs one. Some servers \
         are gateways: the tools they expose search for and run other tools that are not listed up \
         front. For such a server, the loaded tools are not all it offers. When a step needs an \
         outside app or data source, ask the server's search tool for it before you decide no tool \
         exists, and run what it finds through the server's own execute tool."
    );
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
