use axum::http::StatusCode;
use factory_core::{Ask, Command, Job, Stage, Task, Turn};
use maud::{DOCTYPE, Markup, html};

use crate::projects::Projects;
use crate::settings::Settings;
use crate::store::{JobRow, LogEntry};

pub const STYLESHEET: &str = include_str!("../static/style.css");

const STEPS: [&str; 9] = [
    "requested",
    "clarifying",
    "spec",
    "plan",
    "implementing",
    "verifying",
    "pr-open",
    "ci-and-review",
    "merged",
];

fn layout(user: &str, title: &str, body: &Markup) -> Markup {
    page(user, title, false, body)
}

fn page(user: &str, title: &str, refresh: bool, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · factory" }
                @if refresh { meta http-equiv="refresh" content="5"; }
                link rel="stylesheet" href="/style.css";
            }
            body {
                div.topbar {
                    div.topbar-inner {
                        a.brand href="/" { "factory" }
                        div.topbar-end {
                            a.nav href="/settings" { "Settings" }
                            @if !user.is_empty() { span.user { (user) } }
                        }
                    }
                }
                main { (body) }
            }
        }
    }
}

pub fn error(status: StatusCode, message: &str) -> Markup {
    layout(
        "",
        status.as_str(),
        &html! {
            div.card.failed {
                h1 { (status) }
                p { (message) }
                a href="/" { "Back to jobs" }
            }
        },
    )
}

pub fn index(user: &str, projects: &Projects, settings: &Settings, jobs: &[JobRow]) -> Markup {
    let waiting: Vec<&JobRow> = jobs
        .iter()
        .filter(|row| needs_user(&row.job, settings))
        .collect();
    let all: Vec<&JobRow> = jobs.iter().collect();
    layout(
        user,
        "Jobs",
        &html! {
            form.card method="post" action="/jobs" {
                h2 { "New request" }
                div.field {
                    label for="project" { "Project" }
                    select #project name="project" {
                        @for name in projects.names() { option { (name) } }
                    }
                }
                div.field {
                    label for="request" { "What should change?" }
                    textarea #request name="request" required
                        placeholder="Add a reboot button to the device page" {}
                }
                button.primary { "Request" }
            }
            h2.section-title { "Waiting for you" }
            (job_list(&waiting, settings, "Nothing needs you right now."))
            h2.section-title { "All jobs" }
            (job_list(&all, settings, "No jobs yet."))
        },
    )
}

pub fn settings(user: &str, settings: &Settings, notice: Option<&str>) -> Markup {
    layout(
        user,
        "Settings",
        &html! {
            h1 { "Settings" }
            p.meta { "The job loop reads these on every pass. A run that already started keeps the values it started with." }
            @if let Some(notice) = notice {
                div.card.saved { (notice) }
            }
            form.card method="post" action="/settings" {
                h2 { "Workers" }
                div.field {
                    label for="max_workers" { "Workers at a time" }
                    input #max_workers type="number" name="max_workers" min="0" required
                        value=(settings.max_workers);
                    p.hint { "Agent runs and check runs each take one. 0 pauses new runs." }
                }
                div.field {
                    label for="worker_image" { "Image" }
                    input #worker_image.wide type="text" name="worker_image" required
                        value=(settings.worker_image);
                }
                div.row {
                    div.field {
                        label for="worker_memory" { "Memory" }
                        input #worker_memory type="text" name="worker_memory" required
                            value=(settings.worker_memory);
                        p.hint { "As podman's --memory takes it, such as 3g." }
                    }
                    div.field {
                        label for="worker_cpus" { "CPUs" }
                        input #worker_cpus type="text" name="worker_cpus" required
                            value=(settings.worker_cpus);
                    }
                }
                h2 { "GitHub" }
                div.field {
                    label.check {
                        input type="checkbox" name="auto_merge" value="on" checked[settings.auto_merge];
                        " Merge pull requests when their checks pass"
                    }
                    p.hint { "The factory squash-merges once every check passed and no review comment waits for the agent. Off, you merge on GitHub." }
                }
                div.field {
                    label for="poll_seconds" { "Seconds between looks at a pull request" }
                    input #poll_seconds type="number" name="poll_seconds" min="10" required
                        value=(settings.poll_seconds);
                    p.hint { "Also the wait before retrying a failed push or GitHub call." }
                }
                button.primary { "Save" }
            }
        },
    )
}

fn job_list(rows: &[&JobRow], settings: &Settings, empty: &str) -> Markup {
    html! {
        @if rows.is_empty() {
            div.card { p.empty { (empty) } }
        } @else {
            div.card.list-card {
                ul.jobs {
                    @for row in rows {
                        li {
                            span.id { "#" (row.id) }
                            a.title href={ "/jobs/" (row.id) } { (first_line(row.job.request())) }
                            (turn_badge(&row.job, settings))
                            span.sub {
                                (row.project) " · " (stage_label(&row.job)) " · " (row.requested_by)
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn job(user: &str, row: &JobRow, settings: &Settings, logs: &[LogEntry]) -> Markup {
    let job = &row.job;
    let id = row.id;
    page(
        user,
        &format!("Job #{id}"),
        matches!(job.turn(), Turn::Agent(_)),
        &html! {
            h1 { "#" (id) " " (first_line(job.request())) }
            p.meta {
                (row.project) " · requested by " (row.requested_by) " · " (row.created_at) " UTC"
            }
            (steps(job))
            (status_card(id, job, settings))
            div.card {
                h2 { "Request" }
                pre.doc { (job.request()) }
            }
            @if let Some(spec) = job.spec() {
                div.card {
                    h2 { "Spec" }
                    pre.doc { (spec) }
                }
            }
            @if !job.plan().is_empty() {
                div.card {
                    h2 { "Plan" }
                    ol.plan {
                        @for pr in job.plan() {
                            li {
                                strong { (pr.title) }
                                @if let Some(number) = pr.number { " · #" (number) }
                                div.summary { (pr.summary) }
                            }
                        }
                    }
                }
            }
            @if !logs.is_empty() {
                div.card {
                    h2 { "Log" }
                    ol.log {
                        @for entry in logs {
                            li class={ "log-" (entry.kind) } {
                                span.log-at { (entry.at.get(11..).unwrap_or(&entry.at)) }
                                span.log-text { (entry.text) }
                            }
                        }
                    }
                }
            }
            @if *job.turn() != Turn::Done {
                form.footer-actions method="post" action={ "/jobs/" (id) "/cancel" } {
                    button.danger { "Cancel job" }
                }
            }
        },
    )
}

fn step_index(stage: &Stage) -> Option<usize> {
    Some(match stage {
        Stage::Requested => 0,
        Stage::Clarifying => 1,
        Stage::Spec => 2,
        Stage::Plan => 3,
        Stage::Implementing { .. } => 4,
        Stage::Verifying { .. } => 5,
        Stage::PrOpen { .. } => 6,
        Stage::CiAndReview { .. } => 7,
        Stage::Merged => 8,
        Stage::Cancelled | Stage::Failed { .. } => return None,
    })
}

fn steps(job: &Job) -> Markup {
    let Some(current) = step_index(job.stage()) else {
        return html! {};
    };
    let merged = *job.stage() == Stage::Merged;
    html! {
        ol.steps aria-label="Job stages" {
            @for (i, step) in STEPS.iter().enumerate() {
                @let class = if i < current || merged { "done" } else if i == current { "current" } else { "" };
                li class=(class) aria-current=[(i == current).then_some("step")] { (step) }
            }
        }
    }
}

fn status_card(id: i64, job: &Job, settings: &Settings) -> Markup {
    match (job.stage(), job.turn()) {
        (_, Turn::User(Ask::Answers { questions })) => html! {
            form.card.needs-you method="post" action={ "/jobs/" (id) "/answers" } {
                h2 { "The agent has questions" }
                @for (i, question) in questions.iter().enumerate() {
                    div.question {
                        label for={ "answer_" (i) } { (question.text) }
                        textarea id={ "answer_" (i) } name={ "answer_" (i) } required {}
                    }
                }
                button.primary { "Send answers" }
            }
        },
        (_, Turn::User(ask @ (Ask::SpecApproval | Ask::PlanApproval))) => {
            let what = if *ask == Ask::SpecApproval {
                "spec"
            } else {
                "plan"
            };
            html! {
                div.card.needs-you {
                    h2 { "Review the " (what) " below" }
                    div.row {
                        form method="post" action={ "/jobs/" (id) "/approve" } {
                            button.primary { "Approve " (what) }
                        }
                        form.grow method="post" action={ "/jobs/" (id) "/changes" } {
                            textarea name="feedback" required aria-label="What should change"
                                placeholder="Or say what should change" {}
                            p { button { "Request changes" } }
                        }
                    }
                }
            }
        }
        (Stage::Failed { reason }, _) => html! {
            div.card.failed {
                h2 { "Failed" }
                p.empty { (reason) }
            }
        },
        _ => html! {
            div.card {
                (turn_badge(job, settings)) " " (turn_detail(job, settings))
            }
        },
    }
}

/// Whether the job waits for the user: an answer or approval on the
/// dashboard, or, without auto-merge, a review of its open pull request on
/// GitHub.
fn needs_user(job: &Job, settings: &Settings) -> bool {
    matches!(job.turn(), Turn::User(_)) || (in_review(job) && !settings.auto_merge)
}

fn in_review(job: &Job) -> bool {
    matches!(job.next_command(), Some(Command::WatchPr { .. }))
}

fn turn_badge(job: &Job, settings: &Settings) -> Markup {
    let (class, text) = match (job.stage(), job.turn()) {
        (Stage::Merged, _) => ("ok", "merged"),
        (Stage::Failed { .. }, _) => ("bad", "failed"),
        (Stage::Cancelled, _) => ("factory", "cancelled"),
        (Stage::Requested, _) => ("factory", "queued"),
        (_, Turn::User(_)) => ("you", "needs you"),
        _ if in_review(job) && settings.auto_merge => ("factory", "merges when green"),
        _ if in_review(job) => ("you", "in review"),
        (_, Turn::Agent(_)) => ("agent", "agent working"),
        (_, Turn::Factory | Turn::Done) => ("factory", "factory working"),
    };
    html! { span class={ "badge " (class) } { (text) } }
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
    let of = |pr: usize| format!("PR {} of {}", pr + 1, job.plan().len());
    match job.stage() {
        Stage::Requested => "requested".into(),
        Stage::Clarifying => "clarifying".into(),
        Stage::Spec => "spec".into(),
        Stage::Plan => "plan".into(),
        Stage::Implementing { pr } => format!("implementing, {}", of(*pr)),
        Stage::Verifying { pr } => format!("verifying, {}", of(*pr)),
        Stage::PrOpen { pr } => format!("pr-open, {}", of(*pr)),
        Stage::CiAndReview { pr } => format!("ci-and-review, {}", of(*pr)),
        Stage::Merged => "merged".into(),
        Stage::Cancelled => "cancelled".into(),
        Stage::Failed { .. } => "failed".into(),
    }
}

fn turn_detail(job: &Job, settings: &Settings) -> String {
    match job.turn() {
        Turn::Agent(task) => format!("The agent is {}.", task_label(task)),
        Turn::Factory => match job.next_command() {
            Some(Command::RunChecks { .. }) => {
                "The factory is running the project's checks.".into()
            }
            Some(Command::Publish { .. }) => {
                "The factory is pushing the branch and opening the pull request.".into()
            }
            Some(Command::WatchPr { number, .. }) if settings.auto_merge => format!(
                "Auto-merge is on. The factory merges pull request #{number} once all its \
                 checks pass, unless review comments come first, which go to the agent. It \
                 looks every {} seconds.",
                settings.poll_seconds
            ),
            Some(Command::WatchPr { number, .. }) => {
                format!(
                    "Pull request #{number} waits for CI and your review on GitHub. Merge it \
                     when it is ready, or leave review comments and the agent addresses them. \
                     The factory looks every {} seconds.",
                    settings.poll_seconds
                )
            }
            Some(Command::RunAgent { .. }) | None => "Waiting for a free worker.".into(),
        },
        Turn::User(_) => "Waiting for you.".into(),
        Turn::Done => match job.stage() {
            Stage::Merged => "Every planned pull request is merged.".into(),
            _ => "This job is cancelled.".into(),
        },
    }
}

fn task_label(task: &Task) -> &'static str {
    match task {
        Task::Clarify { .. } => "reading the repository",
        Task::WriteSpec => "writing the spec",
        Task::WritePlan => "writing the plan",
        Task::Implement { .. } => "implementing the pull request",
        Task::Answer { .. } => "reading your answers",
        Task::Revise { .. } => "revising after your feedback",
        Task::FixChecks { .. } => "fixing failed checks",
        Task::FixCi { .. } => "fixing the CI failure",
        Task::AddressReview { .. } => "addressing review comments",
        Task::Rebase { .. } => "rebasing onto the newest base branch to resolve a conflict",
    }
}
