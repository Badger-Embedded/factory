use std::fmt::Write;

use factory_core::{
    Agent, Answered, Cli, FileText, Job, PackagingContext, PlannedPr, PrMode, Task, Verdict,
};

use crate::agent::{PromptStyle, backend_for};
use crate::outcome::Expect;

/// Whose wording a prompt carries: an agent's own, or a Local run's on the CLI
/// the factory is set to. A Local run on Codex asks for the schema in the
/// prompt, since it passes no `--output-schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTarget {
    Agent(Agent),
    Local(Cli),
}

impl From<Agent> for PromptTarget {
    fn from(agent: Agent) -> Self {
        PromptTarget::Agent(agent)
    }
}

fn style(target: PromptTarget) -> PromptStyle {
    match target {
        PromptTarget::Agent(agent) => agent
            .cli()
            .and_then(backend_for)
            .expect("the run's CLI has a worker backend")
            .prompt_style(),
        PromptTarget::Local(cli) => backend_for(cli)
            .expect("the run's CLI has a worker backend")
            .local_prompt_style(),
    }
}

/// The prompt for one agent run. `agent` picks the per-harness wording from its
/// backend: the repository instructions file the prompt names and the sentence
/// a review run follows to find its method.
#[must_use]
pub fn prompt(task: &Task, expect: Expect, agent: impl Into<PromptTarget>) -> String {
    prompt_for(task, expect, agent.into(), false)
}

fn rebase_prompt(base: &str, picked: bool) -> String {
    if picked {
        format!(
            "The pull request no longer merges cleanly into {base}. The factory started \
             merging the newest origin/{base} into this checkout. If conflicts remain, \
             resolve them so that both your change and the changes on {base} keep working, \
             stage the files and finish the merge with `git commit --no-edit`. Do not \
             rebase, and do not rewrite or drop any commit, because the pull request holds \
             commits the factory did not write. Run the tests. Do not push. Leave the pull \
             request title and description alone."
        )
    } else {
        format!(
            "The pull request no longer merges cleanly into {base}. The factory fetched \
             the newest origin/{base} into this checkout. Rebase the branch onto \
             origin/{base} and resolve the conflicts so that both your change and the \
             changes on {base} keep working. Run the tests. Do not push. Update the pull \
             request title and description if the rebase changed what they describe."
        )
    }
}

/// [`prompt`], for a job on a picked pull request when `picked` is set. The
/// branch then holds commits the factory did not write and the pull request
/// is not the factory's to retitle, so the prompt says to keep both.
#[allow(
    clippy::too_many_lines,
    reason = "one match arm per task, as in `job_body`"
)]
fn prompt_for(task: &Task, expect: Expect, agent: PromptTarget, picked: bool) -> String {
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
                 your work on the current branch. Do not push or open a pull request.",
                style(agent).rules_doc
            );
            if picked {
                text.push_str(PICKED_NOTE);
            } else {
                text.push_str(
                    " The factory does that with the title and description you return, so \
                     write them the way CONTRIBUTING.md asks.",
                );
            }
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
        Task::Rebase { base } => rebase_prompt(base, picked),
        Task::MergeHead => "Someone pushed to the pull request's branch while you worked. The \
             factory started merging their commits into this checkout and the merge \
             conflicts. Resolve the conflicts so that both their change and yours keep \
             working, stage the files and finish the merge with `git commit --no-edit`. Do \
             not rebase, and do not rewrite or drop any commit. Run the tests. Do not push. \
             Leave the pull request title and description alone."
            .to_owned(),
        Task::AddressReview { comments } => format!(
            "The reviewer left comments on the pull request:\n\n{comments}\n\n\
             Address them and commit."
        ),
        Task::Review => {
            return review_prompt(&ReviewContext::default(), agent);
        }
        Task::QuickReview => {
            return quick_review_prompt(&ReviewContext::default(), agent);
        }
        Task::AnalyzeFailure { log } => {
            return failure_prompt(log, agent);
        }
        Task::CheckDiff { diff, body } => return diff_prompt(diff, body, agent),
        Task::FindGaps { context } => return gaps_prompt(context, agent),
    };
    if picked && matches!(expect, Expect::Commits | Expect::FixCi) {
        text.push_str(if matches!(agent, PromptTarget::Agent(Agent::Codex)) {
            PICKED_RETURN_NOTE_STRICT
        } else {
            PICKED_RETURN_NOTE
        });
    }
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

const PICKED_NOTE: &str = " The branch holds an existing pull request. Add commits on top \
     of it. Do not rewrite, drop or amend a commit that was already there, and do not rebase.";

const PICKED_RETURN_NOTE: &str = "\n\nThe pull request's title and description stay as they are, \
     so you may leave `pull_request` out of the structured output.";

const PICKED_RETURN_NOTE_STRICT: &str = "\n\nThe pull request's title and description stay as \
     they are, so return `\"pull_request\": null`.";

fn end_prompt(text: &mut String, agent: PromptTarget, expect: Expect) {
    if let Some(fields) = output_fields(expect) {
        text.push_str("\n\n");
        text.push_str(&fields);
    }
    text.push_str("\n\n");
    text.push_str(NIX_PARAGRAPH);
    append_schema(text, agent, expect);
}

fn output_fields(expect: Expect) -> Option<String> {
    let schema = expect.schema();
    let required: Vec<&str> = schema["required"]
        .as_array()?
        .iter()
        .filter_map(|name| name.as_str())
        .collect();
    if required == ["outcome"] {
        return None;
    }
    let optional: Vec<&str> = schema["properties"]
        .as_object()
        .map(|properties| {
            properties
                .keys()
                .map(String::as_str)
                .filter(|name| !required.contains(name))
                .collect()
        })
        .unwrap_or_default();
    let list = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "The structured output must have these required top-level fields, each its own \
         field: {}. Do not put one field's content inside another.",
        list(&required)
    );
    if !optional.is_empty() {
        let _ = write!(text, " These fields are optional: {}.", list(&optional));
    }
    Some(text)
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
fn append_schema(text: &mut String, agent: PromptTarget, expect: Expect) {
    if let Some(instruction) = style(agent).schema_in_prompt {
        let schema = serde_json::to_string_pretty(&expect.schema())
            .unwrap_or_else(|_| expect.schema().to_string());
        let _ = write!(text, "\n\n{instruction}\n\n{schema}");
    }
}

fn failure_prompt(log: &str, agent: PromptTarget) -> String {
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
fn diff_prompt(diff: &str, body: &str, agent: PromptTarget) -> String {
    let mut text = format!(
        "This is a Renovate pull request, and all its checks passed. Its description:\n\n\
         {body}\n\n\
         Its diff:\n\n{diff}\n\n\
         Decide whether the diff is safe to merge without a person reading it. The diff is \
         safe when every change only moves a version, in any file, and nothing in the new \
         version breaks this project. A version move is a version number, tag, ref, commit \
         SHA or digest pin, with its lockfile or checksum lines. The kind of file does not \
         decide: workflows, Dockerfiles, scripts and configuration are fine when only a \
         version moves. Any other change is unsafe, such as edited source code, an added or \
         removed step, or a changed command, trigger, permission, option or setting. A diff \
         with any such change is always unsafe, even when it also moves versions, and \
         compatibility never makes it safe.\n\n\
         For a pure version bump, compatibility decides. Read the release notes in the \
         description. For each breaking change they list, check the repository for use of \
         what it changes, for example whether a workflow has a trigger the change affects. \
         If the project is not affected, the update is safe. If it is affected, or you \
         cannot tell, the update is unsafe. When the description lists no breaking \
         change, a pure version bump is safe, a major bump included. Decide this yourself: \
         do not answer unsafe only so that a person can confirm something you can read in \
         the repository or the diff. Do not change any files and do not push.\n\n\
         End with outcome \"safe\" or \"unsafe\", a reason and a short summary of the \
         update. For safe, the reason names the files and, when the release notes list \
         breaking changes, says why each does not affect the project. For unsafe, the \
         reason names the change that is not a version move. When a breaking change \
         stops the update, it names that change and where the project uses it, or, when \
         you cannot tell, what you could not determine. When the diff shows the upstream versions or refs the \
         update moves between, give them as \"from\" and \"to\"."
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
fn gaps_prompt(context: &PackagingContext, agent: PromptTarget) -> String {
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
    /// The picked pull request, for a review of one.
    pr: Option<PickedContext<'a>>,
}

/// What a review of a picked pull request reads besides the diff.
struct PickedContext<'a> {
    title: &'a str,
    body: &'a str,
    /// The pull request's own base branch, which the diff is read against.
    base: &'a str,
    /// The text the user typed in the form, as extra instructions. Empty
    /// when there is none or on a later pass, whose session holds it.
    focus: &'a str,
}

impl<'a> ReviewContext<'a> {
    fn for_job(job: &'a Job) -> Self {
        let stack = job
            .stacked_base()
            .zip(job.plan().first())
            .filter(|(_, planned)| planned.number.is_some());
        let pr = match stack {
            // Once the stack is open, the reviewer reads it, not the pull
            // request it fixes.
            Some((base, planned)) => Some(PickedContext {
                title: planned
                    .description
                    .as_ref()
                    .map_or(&planned.title, |description| &description.title),
                body: planned
                    .description
                    .as_ref()
                    .map_or(&planned.summary, |description| &description.body),
                base,
                focus: "",
            }),
            None => job
                .picked_pr()
                .filter(|picked| !matches!(picked.mode, PrMode::Request))
                .map(|picked| PickedContext {
                    title: &picked.title,
                    body: &picked.body,
                    base: &picked.base_branch,
                    focus: "",
                }),
        };
        let mut context = Self::for_stage(job);
        if let Some(mut pr) = pr {
            if context.previous.is_none() && context.new_answers.is_empty() {
                pr.focus = job.request().trim();
            }
            // The request is the review's focus, so the decisions block that
            // restates it as a feature request is left out.
            context.decisions.clear();
            context.pr = Some(pr);
        }
        context
    }

    fn for_stage(job: &'a Job) -> Self {
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
            ..ReviewContext::default()
        }
    }
}

/// The prompt for an LLM review run. The worker has no GitHub token, so the
/// reviewer reads the branch's diff from the checkout and does not push. The
/// session resumes across passes, so the first pass on a pull request gets the
/// request, answers, spec and plan, and a later pass gets only the answers
/// given since, with the feedback it asked for when it asked for changes.
fn review_prompt(context: &ReviewContext, agent: PromptTarget) -> String {
    let style = style(agent);
    let mut text = context.decisions.clone();
    if let Some(pr) = &context.pr {
        picked_section(&mut text, pr);
    }
    let _ = write!(
        text,
        "Review the commits this branch adds on top of its base. {} Check the change for \
         correctness and for the repository's own rules in {} and CONTRIBUTING.md. Do not \
         change any files and do not push.\n\n",
        diff_sentence(context.pr.as_ref()),
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

/// The pull request's title and description, and the user's instructions for
/// the review when there are any.
fn picked_section(text: &mut String, pr: &PickedContext) {
    let _ = write!(text, "The pull request is titled:\n\n{}\n\n", pr.title);
    if pr.body.trim().is_empty() {
        text.push_str("Its description is empty.\n\n");
    } else {
        let _ = write!(text, "Its description:\n\n{}\n\n", pr.body);
    }
    if !pr.focus.is_empty() {
        let _ = write!(
            text,
            "The requester gave these extra instructions for the review, such as what to \
             focus on:\n\n{}\n\n",
            pr.focus
        );
    }
}

/// How the reviewer reads the diff. A picked pull request is read against its
/// own base branch, which is not always the project's.
fn diff_sentence(pr: Option<&PickedContext>) -> String {
    match pr {
        Some(pr) => format!(
            "Read the diff with `git diff {range}`, since the pull request merges into \
             {base}.",
            range = shell_quote(&format!("refs/remotes/origin/{}...HEAD", pr.base)),
            base = pr.base
        ),
        None => "Read the diff with git, against the base branch the checkout is on.".to_owned(),
    }
}

/// Quotes `text` as one shell word, so a branch name with metacharacters
/// cannot break the command.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The prompt for the quick review of a picked pull request. It reads the
/// pull request's diff from the checkout, as the review loop does, and ends
/// with a score and the review text, with no verdict and no question.
fn quick_review_prompt(context: &ReviewContext, agent: PromptTarget) -> String {
    let style = style(agent);
    let mut text = String::new();
    if let Some(pr) = &context.pr {
        picked_section(&mut text, pr);
    }
    let _ = write!(
        text,
        "Review the pull request in the current checkout. {} Check the change for \
         correctness and for the repository's own rules in {} and CONTRIBUTING.md. Do not \
         change any files and do not push.\n\n",
        diff_sentence(context.pr.as_ref()),
        style.rules_doc
    );
    text.push_str(style.review_method);
    text.push_str(
        "The review skill ends with an approve or changes verdict. Ignore that part: this \
         review ends with the score and the review text described below, and has no \
         \"outcome\" field.\n\n\
         Give the pull request a score from 1 to 5 for how safe it is to merge as it is. \
         5 means it is safe to merge. 1 means do not merge. The scores between are your \
         judgment of how much is wrong and how serious it is: a change with only small \
         problems scores 4, and a change with a defect that must be fixed before a merge \
         scores 2 or lower.\n\n\
         End with the structured output. Its \"score\" is the whole number and its \
         \"review\" is the whole review as Markdown. The review gives the reason for the \
         score and lists each problem with its file and line, concrete enough to act on \
         without the diff in view. The requester reads it and decides what happens next.",
    );
    end_prompt(&mut text, agent, Expect::QuickReview);
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
    agent: impl Into<PromptTarget>,
    mcp_servers: &[String],
) -> String {
    let mut text = job_body(job, task, expect, agent.into());
    append_mcp(&mut text, mcp_servers);
    text
}

fn job_body(job: &Job, task: &Task, expect: Expect, agent: PromptTarget) -> String {
    if let Task::QuickReview = task {
        return quick_review_prompt(&ReviewContext::for_job(job), agent);
    }
    if let Task::Review = task {
        return review_prompt(&ReviewContext::for_job(job), agent);
    }
    let mut body = prompt_for(
        task,
        expect,
        agent,
        job.picked_pr().is_some() && job.stacked_on().is_none(),
    );
    // A fork's stack carries the fork author's commits, so a conflict is
    // merged, never rebased.
    if let (Some(picked), Task::Rebase { base }) = (job.stacked_on(), task)
        && picked.fork
    {
        body = rebase_prompt(base, true);
    }
    if let (Some(reviewed), Task::AddressReview { .. }) = (job.stacked_on(), task) {
        body = format!(
            "This branch starts at the head of pull request #{} ({}). Your commits go in a \
             new pull request that fixes it, so write the title and description you return \
             for the fixes, not for the whole reviewed pull request.\n\n{body}",
            reviewed.number, reviewed.title
        );
    }
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
