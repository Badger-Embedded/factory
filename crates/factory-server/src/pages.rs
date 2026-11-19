use axum::http::StatusCode;
use factory_core::{Ask, Command, Job, Session, Stage, Task, Turn};
use maud::{DOCTYPE, Markup, html};

use crate::projects::Projects;
use crate::store::JobRow;

const STYLE: &str = "
body { font: 15px/1.5 system-ui, sans-serif; max-width: 60rem; margin: 0 auto; padding: 1rem;
       color: #1d1d1f; background: #fff; }
@media (prefers-color-scheme: dark) { body { color: #e8e8ea; background: #161618; } }
header { display: flex; justify-content: space-between; align-items: baseline; }
header a { color: inherit; text-decoration: none; font-weight: 600; }
table { border-collapse: collapse; width: 100%; }
td, th { text-align: left; padding: .35rem .5rem; border-bottom: 1px solid #8884; }
textarea { width: 100%; box-sizing: border-box; min-height: 5rem; font: inherit; }
pre { white-space: pre-wrap; background: #8881; padding: .75rem; border-radius: 4px; }
.muted { color: #888; }
.actions { display: flex; gap: .5rem; flex-wrap: wrap; align-items: flex-start; }
form { margin: .5rem 0; }
";

fn layout(user: &str, title: &str, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · factory" }
                style { (STYLE) }
            }
            body {
                header {
                    a href="/" { "factory" }
                    span.muted { (user) }
                }
                (body)
            }
        }
    }
}

pub fn error(status: StatusCode, message: &str) -> Markup {
    layout(
        "",
        status.as_str(),
        &html! {
            h1 { (status) }
            p { (message) }
            p { a href="javascript:history.back()" { "Back" } }
        },
    )
}

pub fn index(user: &str, projects: &Projects, jobs: &[JobRow]) -> Markup {
    let waiting: Vec<&JobRow> = jobs
        .iter()
        .filter(|row| matches!(row.job.turn(), Turn::User(_)))
        .collect();
    layout(
        user,
        "Jobs",
        &html! {
            h2 { "New request" }
            form method="post" action="/jobs" {
                p {
                    label { "Project " }
                    select name="project" {
                        @for name in projects.names() { option { (name) } }
                    }
                }
                textarea name="request" required placeholder="Describe the change you want" {}
                p { button { "Request" } }
            }
            h2 { "Waiting for you" }
            @if waiting.is_empty() {
                p.muted { "Nothing." }
            } @else {
                (job_table(&waiting))
            }
            h2 { "All jobs" }
            @if jobs.is_empty() {
                p.muted { "No jobs yet." }
            } @else {
                (job_table(&jobs.iter().collect::<Vec<_>>()))
            }
        },
    )
}

fn job_table(rows: &[&JobRow]) -> Markup {
    html! {
        table {
            tr { th { "Job" } th { "Project" } th { "Request" } th { "Stage" } th { "Waiting on" } }
            @for row in rows {
                tr {
                    td { a href={ "/jobs/" (row.id) } { "#" (row.id) } }
                    td { (row.project) }
                    td { (first_line(row.job.request())) }
                    td { (stage_label(&row.job)) }
                    td { (turn_label(&row.job)) }
                }
            }
        }
    }
}

pub fn job(user: &str, row: &JobRow) -> Markup {
    let job = &row.job;
    let id = row.id;
    layout(
        user,
        &format!("Job #{id}"),
        &html! {
            h1 { "#" (id) " " (first_line(job.request())) }
            p.muted {
                (row.project) " · requested by " (row.requested_by) " · " (row.created_at) " UTC"
            }
            p { b { "Stage: " } (stage_label(job)) " · " b { "Waiting on: " } (turn_label(job)) }
            h2 { "Request" }
            pre { (job.request()) }
            (pending(id, job))
            @if let Some(spec) = job.spec() {
                h2 { "Spec" }
                pre { (spec) }
            }
            @if !job.plan().is_empty() {
                h2 { "Plan" }
                ol {
                    @for pr in job.plan() {
                        li {
                            b { (pr.title) }
                            @if let Some(number) = pr.number { " · #" (number) }
                            br; (pr.summary)
                        }
                    }
                }
            }
            @if *job.turn() != Turn::Done {
                form method="post" action={ "/jobs/" (id) "/cancel" } {
                    button { "Cancel job" }
                }
            }
        },
    )
}

fn pending(id: i64, job: &Job) -> Markup {
    match job.turn() {
        Turn::User(Ask::Answers { questions }) => html! {
            h2 { "Questions" }
            form method="post" action={ "/jobs/" (id) "/answers" } {
                @for (i, question) in questions.iter().enumerate() {
                    p { label for={ "answer_" (i) } { (question.text) } }
                    textarea id={ "answer_" (i) } name={ "answer_" (i) } required {}
                }
                p { button { "Send answers" } }
            }
        },
        Turn::User(ask @ (Ask::SpecApproval | Ask::PlanApproval)) => {
            let what = if *ask == Ask::SpecApproval {
                "spec"
            } else {
                "plan"
            };
            html! {
                h2 { "Approve the " (what) }
                div.actions {
                    form method="post" action={ "/jobs/" (id) "/approve" } {
                        button { "Approve " (what) }
                    }
                    form method="post" action={ "/jobs/" (id) "/changes" } style="flex: 1" {
                        textarea name="feedback" required placeholder="What should change" {}
                        button { "Request changes" }
                    }
                }
            }
        }
        _ => html! {},
    }
}

fn first_line(text: &str) -> String {
    const MAX: usize = 80;
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > MAX {
        format!("{}…", line.chars().take(MAX).collect::<String>())
    } else {
        line.to_owned()
    }
}

fn stage_label(job: &Job) -> String {
    let of = |pr: usize| format!("{}/{}", pr + 1, job.plan().len());
    match job.stage() {
        Stage::Requested => "requested".into(),
        Stage::Clarifying => "clarifying".into(),
        Stage::Spec => "spec".into(),
        Stage::Plan => "plan".into(),
        Stage::Implementing { pr } => format!("implementing {}", of(*pr)),
        Stage::Verifying { pr } => format!("verifying {}", of(*pr)),
        Stage::PrOpen { pr } => format!("pr-open {}", of(*pr)),
        Stage::CiAndReview { pr } => format!("ci-and-review {}", of(*pr)),
        Stage::Merged => "merged".into(),
        Stage::Cancelled => "cancelled".into(),
        Stage::Failed { reason } => format!("failed: {reason}"),
    }
}

fn turn_label(job: &Job) -> String {
    match job.turn() {
        Turn::Agent(task) => format!("agent, {}", task_label(task)),
        Turn::User(Ask::Answers { questions }) => {
            format!("you, {} question(s)", questions.len())
        }
        Turn::User(Ask::SpecApproval) => "you, spec approval".into(),
        Turn::User(Ask::PlanApproval) => "you, plan approval".into(),
        Turn::Factory => match job.next_command() {
            Some(Command::RunChecks { .. }) => "factory, running checks".into(),
            Some(Command::Publish { .. }) => "factory, opening the pull request".into(),
            Some(Command::WatchPr { number, .. }) => format!("factory, watching #{number}"),
            Some(Command::RunAgent { session, .. }) => match session {
                Session::Planning => "factory, planning session".into(),
                Session::Pr(pr) => format!("factory, session for pull request {}", pr + 1),
            },
            None => "factory, a free worker".into(),
        },
        Turn::Done => "nobody".into(),
    }
}

fn task_label(task: &Task) -> &'static str {
    match task {
        Task::Clarify { .. } => "reading the repository",
        Task::WriteSpec => "writing the spec",
        Task::WritePlan => "writing the plan",
        Task::Implement { .. } => "implementing",
        Task::Answer { .. } => "reading your answers",
        Task::Revise { .. } => "revising",
        Task::FixChecks { .. } => "fixing failed checks",
        Task::FixCi { .. } => "fixing CI",
        Task::AddressReview { .. } => "addressing review comments",
    }
}
